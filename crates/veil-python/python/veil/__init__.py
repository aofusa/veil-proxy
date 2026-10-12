"""veil API サーバライブラリの Python バインディング（F-194）。

FastAPI 風のデコレータ API。HTTP の受理・TLS・HTTP/2/3・OpenAPI 配信は Rust 側（veil）が行い、
Python はハンドラ関数だけを提供する。ネイティブ拡張 ``veil._veil`` は ``run()`` / ``shutdown()`` で
初めて読み込む（引数束縛・検証・OpenAPI 生成は純 Python で、拡張が無くてもテストできる）。

    import veil

    @veil.get("/hello/{name}")
    def hello(name: str, greeting: str = "Hello"):
        return {"message": f"{greeting}, {name}"}

    veil.run(listen="127.0.0.1:8000")
"""

import asyncio
import http
import inspect
import json
import re
import typing
import urllib.parse

__all__ = [
    "App",
    "Request",
    "Response",
    "JSONResponse",
    "HTTPException",
    "Headers",
    "route",
    "get",
    "post",
    "put",
    "delete",
    "patch",
    "run",
    "shutdown",
]

_EMPTY = inspect.Parameter.empty
_JSON_CT = "application/json"


# --------------------------------------------------------------------------
# リクエスト・レスポンス
# --------------------------------------------------------------------------


class Headers:
    """大文字小文字を区別しない読み取り専用のヘッダ辞書（同名ヘッダは受信順に保持）。"""

    def __init__(self, items=()):
        self._items = [(str(k), str(v)) for k, v in items]

    def get(self, name, default=None):
        low = name.lower()
        for k, v in self._items:
            if k.lower() == low:
                return v
        return default

    def getlist(self, name):
        low = name.lower()
        return [v for k, v in self._items if k.lower() == low]

    def __getitem__(self, name):
        v = self.get(name)
        if v is None:
            raise KeyError(name)
        return v

    def __contains__(self, name):
        return self.get(name) is not None

    def __iter__(self):
        return iter(self.keys())

    def __len__(self):
        return len(self._items)

    def keys(self):
        seen = []
        for k, _ in self._items:
            if k.lower() not in [s.lower() for s in seen]:
                seen.append(k)
        return seen

    def items(self):
        return list(self._items)

    def __repr__(self):
        return "Headers(%r)" % (self._items,)


class Request:
    """ハンドラに渡るリクエスト。"""

    def __init__(self, method="GET", path="/", query="", headers=(), body=b"", params=None, client_ip=""):
        self.method = method
        self.path = path
        self.query = query
        self.headers = headers if isinstance(headers, Headers) else Headers(headers)
        self.body = body
        self.params = dict(params or {})
        self.client_ip = client_ip
        self._query_params = None

    @property
    def query_params(self):
        """クエリの辞書（同名キーは最初の値）。"""
        if self._query_params is None:
            parsed = urllib.parse.parse_qs(self.query, keep_blank_values=True)
            self._query_params = {k: v[0] for k, v in parsed.items()}
        return self._query_params

    def json(self):
        return json.loads(self.body)

    def text(self):
        return self.body.decode("utf-8", errors="replace")


def _json_default(o):
    if hasattr(o, "model_dump"):
        return o.model_dump(mode="json")
    if hasattr(o, "isoformat"):
        return o.isoformat()
    return str(o)


def _dumps(obj):
    return json.dumps(obj, separators=(",", ":"), ensure_ascii=False, default=_json_default).encode("utf-8")


class Response:
    """生のレスポンス。``body`` は bytes / str。"""

    def __init__(self, body=b"", status=200, headers=None, media_type=None):
        if isinstance(body, str):
            body = body.encode("utf-8")
        self.body = bytes(body)
        self.status = int(status)
        if headers is None:
            self.headers = []
        elif isinstance(headers, dict):
            self.headers = [(str(k), str(v)) for k, v in headers.items()]
        else:
            self.headers = [(str(k), str(v)) for k, v in headers]
        if media_type is not None and not any(k.lower() == "content-type" for k, _ in self.headers):
            self.headers.append(("content-type", media_type))

    def _triple(self):
        return (self.status, self.headers, self.body)


class JSONResponse(Response):
    def __init__(self, content=None, status=200, headers=None):
        super().__init__(_dumps(content), status, headers, _JSON_CT)


class HTTPException(Exception):
    """``raise HTTPException(404, "not found")`` → ``{"detail": "not found"}``。"""

    def __init__(self, status_code, detail=None, headers=None):
        if detail is None:
            try:
                detail = http.HTTPStatus(status_code).phrase
            except ValueError:
                detail = ""
        super().__init__(status_code, detail)
        self.status_code = status_code
        self.detail = detail
        self.headers = headers


class _ValidationFailed(Exception):
    def __init__(self, errors):
        super().__init__(errors)
        self.errors = errors


def _err(loc, msg, typ):
    return {"loc": list(loc), "msg": msg, "type": typ}


# --------------------------------------------------------------------------
# 型注釈の扱い（pydantic は import しない。メソッドの有無で判定する）
# --------------------------------------------------------------------------


def _is_model(ann):
    return (
        isinstance(ann, type)
        and hasattr(ann, "model_validate_json")
        and hasattr(ann, "model_json_schema")
    )


def _unwrap_optional(ann):
    """``Optional[X]`` → (X, True)。それ以外は (ann, False)。"""
    if typing.get_origin(ann) is typing.Union:
        args = [a for a in typing.get_args(ann) if a is not type(None)]
        if len(args) == 1 and len(typing.get_args(ann)) == 2:
            return args[0], True
    return ann, False


def _is_container(ann):
    return ann in (dict, list) or typing.get_origin(ann) in (dict, list)


def _scalar_schema(ann):
    if ann is bool:
        return {"type": "boolean"}
    if ann is int:
        return {"type": "integer"}
    if ann is float:
        return {"type": "number"}
    return {"type": "string"}


_BOOL_TRUE = ("true", "1", "yes", "on", "t", "y")
_BOOL_FALSE = ("false", "0", "no", "off", "f", "n")


def _convert_scalar(value, ann):
    """文字列を注釈（int/float/bool/str）で変換する。失敗は ValueError(msg, type)。"""
    if ann is int:
        try:
            return int(value)
        except ValueError:
            raise ValueError("Input should be a valid integer, unable to parse string as an integer", "int_parsing")
    if ann is float:
        try:
            return float(value)
        except ValueError:
            raise ValueError("Input should be a valid number, unable to parse string as a number", "float_parsing")
    if ann is bool:
        low = value.lower()
        if low in _BOOL_TRUE:
            return True
        if low in _BOOL_FALSE:
            return False
        raise ValueError("Input should be a valid boolean, unable to interpret input", "bool_parsing")
    return value


def _resolve_hints(func):
    try:
        return typing.get_type_hints(func)
    except Exception:
        return {}


_STR_ANN = {"int": int, "float": float, "bool": bool, "str": str, "dict": dict, "list": list}


class _Param:
    __slots__ = ("name", "kind", "ann", "model", "default", "required")

    def __init__(self, name, kind, ann, model, default):
        self.name = name
        self.kind = kind  # request / path / body / query
        self.ann = ann
        self.model = model
        self.default = default
        self.required = default is _EMPTY


def _build_params(func, path_names):
    hints = _resolve_hints(func)
    out = []
    for name, p in inspect.signature(func).parameters.items():
        if p.kind in (p.VAR_POSITIONAL, p.VAR_KEYWORD):
            continue
        ann = hints.get(name, p.annotation)
        if isinstance(ann, str):
            ann = _STR_ANN.get(ann, _EMPTY)
        base, _ = _unwrap_optional(ann)
        if name in ("req", "request") or (isinstance(base, type) and issubclass(base, Request)):
            out.append(_Param(name, "request", base, None, p.default))
        elif name in path_names:
            out.append(_Param(name, "path", base, None, p.default))
        elif _is_model(base):
            out.append(_Param(name, "body", base, base, p.default))
        elif name == "body" or _is_container(base):
            out.append(_Param(name, "body", base, None, p.default))
        else:
            out.append(_Param(name, "query", base, None, p.default))
    return out


# --------------------------------------------------------------------------
# body_constraints（pydantic 無しの検証と OpenAPI 反映）
# --------------------------------------------------------------------------

_TYPE_CHECKS = {
    "string": lambda v: isinstance(v, str),
    "integer": lambda v: isinstance(v, int) and not isinstance(v, bool),
    "number": lambda v: isinstance(v, (int, float)) and not isinstance(v, bool),
    "boolean": lambda v: isinstance(v, bool),
    "array": lambda v: isinstance(v, list),
    "object": lambda v: isinstance(v, dict),
    "null": lambda v: v is None,
}


def _is_num(v):
    return isinstance(v, (int, float)) and not isinstance(v, bool)


def _field_rules(rules):
    """OpenAPI に出す規則（フィールド単位の ``required`` は除く）。"""
    return {k: v for k, v in rules.items() if k != "required"}


def validate_constraints(obj, constraints):
    """辞書の本文を ``body_constraints`` で検証し、FastAPI 形のエラー配列を返す（空なら OK）。"""
    errors = []
    if not isinstance(obj, dict):
        return [_err(["body"], "Input should be a valid dictionary or object", "dict_type")]
    top_required = constraints.get("required")
    if isinstance(top_required, (list, tuple)):
        for f in top_required:
            if f not in obj:
                errors.append(_err(["body", f], "Field required", "missing"))
    for field, rules in constraints.items():
        if field == "required" and isinstance(rules, (list, tuple)):
            continue
        if not isinstance(rules, dict):
            continue
        if field not in obj:
            if rules.get("required") is True:
                errors.append(_err(["body", field], "Field required", "missing"))
            continue
        errors.extend(_check_value(obj[field], rules, ["body", field]))
    return errors


def _check_value(v, rules, loc):
    errs = []
    t = rules.get("type")
    if t is not None:
        check = _TYPE_CHECKS.get(t)
        if check is not None and not check(v):
            return [_err(loc, "Input should be a valid %s" % t, "%s_type" % t)]
    if "enum" in rules and v not in rules["enum"]:
        errs.append(_err(loc, "Input should be one of %s" % (", ".join(repr(e) for e in rules["enum"]),), "enum"))
    if _is_num(v):
        if "minimum" in rules and v < rules["minimum"]:
            errs.append(_err(loc, "Input should be greater than or equal to %s" % rules["minimum"], "greater_than_equal"))
        if "maximum" in rules and v > rules["maximum"]:
            errs.append(_err(loc, "Input should be less than or equal to %s" % rules["maximum"], "less_than_equal"))
        if "exclusiveMinimum" in rules and v <= rules["exclusiveMinimum"]:
            errs.append(_err(loc, "Input should be greater than %s" % rules["exclusiveMinimum"], "greater_than"))
        if "exclusiveMaximum" in rules and v >= rules["exclusiveMaximum"]:
            errs.append(_err(loc, "Input should be less than %s" % rules["exclusiveMaximum"], "less_than"))
    if isinstance(v, str):
        if "minLength" in rules and len(v) < rules["minLength"]:
            errs.append(_err(loc, "String should have at least %d characters" % rules["minLength"], "string_too_short"))
        if "maxLength" in rules and len(v) > rules["maxLength"]:
            errs.append(_err(loc, "String should have at most %d characters" % rules["maxLength"], "string_too_long"))
        if "pattern" in rules and re.fullmatch(rules["pattern"], v) is None:
            errs.append(_err(loc, "String should match pattern '%s'" % rules["pattern"], "string_pattern_mismatch"))
    return errs


def constraints_schema(constraints):
    """``body_constraints`` を OpenAPI の JSON Schema（object）にする。"""
    props = {}
    required = []
    top = constraints.get("required")
    if isinstance(top, (list, tuple)):
        required.extend(top)
    for field, rules in constraints.items():
        if field == "required" and isinstance(rules, (list, tuple)):
            continue
        if not isinstance(rules, dict):
            continue
        props[field] = _field_rules(rules)
        if rules.get("required") is True and field not in required:
            required.append(field)
    for f in required:
        props.setdefault(f, {})
    schema = {"type": "object", "properties": props}
    if required:
        schema["required"] = required
    return schema


# --------------------------------------------------------------------------
# ルート（引数束縛・戻り値変換・OpenAPI）
# --------------------------------------------------------------------------

_PATH_NAME_RE = re.compile(r"\{\*?([^}/:]+)(?::[^}]*)?\}")


def _model_errors(exc, prefix):
    out = []
    for e in exc.errors():
        out.append(_err(list(prefix) + list(e.get("loc", ())), str(e.get("msg", "")), str(e.get("type", "value_error"))))
    return out


def _is_validation_error(exc):
    return type(exc).__name__ == "ValidationError" and hasattr(exc, "errors")


class Route:
    """登録された 1 ルート。``handle()`` は Rust 非依存で単体テストできる。"""

    def __init__(self, func, path, methods, offload, body_constraints, tags, summary, status_code):
        self.func = func
        self.path = path
        self.methods = [m.upper() for m in methods]
        self.offload = offload
        self.body_constraints = body_constraints
        self.tags = list(tags) if tags else []
        self.summary = summary
        self.status_code = status_code
        self.path_names = _PATH_NAME_RE.findall(path)
        self.params = _build_params(func, set(self.path_names))
        self.is_async = inspect.iscoroutinefunction(func)
        self._hints = _resolve_hints(func)

    # ---- 引数束縛 ----

    def bind(self, req):
        """リクエストから関数の引数を作る。検証エラーは ``_ValidationFailed``。"""
        kwargs = {}
        errors = []
        qp = None
        body_checked = False
        for p in self.params:
            if p.kind == "request":
                kwargs[p.name] = req
            elif p.kind == "path":
                raw = req.params.get(p.name)
                if raw is None:
                    errors.append(_err(["path", p.name], "Field required", "missing"))
                    continue
                try:
                    kwargs[p.name] = _convert_scalar(raw, p.ann)
                except ValueError as e:
                    errors.append(_err(["path", p.name], e.args[0], e.args[1]))
            elif p.kind == "query":
                if qp is None:
                    qp = req.query_params
                if p.name in qp:
                    try:
                        kwargs[p.name] = _convert_scalar(qp[p.name], p.ann)
                    except ValueError as e:
                        errors.append(_err(["query", p.name], e.args[0], e.args[1]))
                elif p.required:
                    errors.append(_err(["query", p.name], "Field required", "missing"))
                # 既定値があれば関数側の既定値に任せる
            elif p.kind == "body":
                body_checked = True
                self._bind_body(p, req, kwargs, errors)
        if self.body_constraints and not body_checked:
            self._bind_body(None, req, kwargs, errors)
        if errors:
            raise _ValidationFailed(errors)
        return kwargs

    def _bind_body(self, p, req, kwargs, errors):
        required = p is None or p.required
        if not req.body.strip():
            if required:
                errors.append(_err(["body"], "Field required", "missing"))
            return
        try:
            if p is not None and p.model is not None:
                try:
                    value = p.model.model_validate_json(req.body)
                except Exception as e:  # pydantic.ValidationError を import せずに判定
                    if _is_validation_error(e):
                        errors.extend(_model_errors(e, ["body"]))
                        return
                    raise
            else:
                try:
                    value = json.loads(req.body)
                except ValueError as e:
                    errors.append(_err(["body"], "JSON decode error: %s" % e, "json_invalid"))
                    return
                if self.body_constraints:
                    errs = validate_constraints(value, self.body_constraints)
                    if errs:
                        errors.extend(errs)
                        return
        except UnicodeDecodeError:
            errors.append(_err(["body"], "JSON decode error", "json_invalid"))
            return
        if p is not None:
            kwargs[p.name] = value

    # ---- 戻り値変換 ----

    def to_response(self, result):
        status = self.status_code
        extra = None
        if isinstance(result, Response):
            return result
        if isinstance(result, tuple) and len(result) in (2, 3):
            if len(result) == 3:
                result, status, extra = result
            else:
                result, status = result
            if isinstance(result, Response):
                return result
        if result is None:
            r = Response(b"", 204 if status == 200 else status)
        elif isinstance(result, (bytes, bytearray)):
            r = Response(bytes(result), status, media_type="application/octet-stream")
        elif isinstance(result, str):
            r = Response(result, status, media_type="text/plain; charset=utf-8")
        elif hasattr(result, "model_dump_json"):
            r = Response(result.model_dump_json(), status, media_type=_JSON_CT)
        else:
            r = JSONResponse(result, status)
        if extra:
            r.headers.extend(Response(b"", headers=extra).headers)
        return r

    def handle(self, req):
        """リクエスト → ``Response``（HTTPException・検証エラーはここで応答にする）。"""
        try:
            kwargs = self.bind(req)
            result = self.func(**kwargs)
            if inspect.iscoroutine(result):
                result = asyncio.run(result)
            return self.to_response(result)
        except _ValidationFailed as e:
            return JSONResponse({"detail": e.errors}, 422)
        except HTTPException as e:
            return JSONResponse({"detail": e.detail}, e.status_code, e.headers)

    # ---- OpenAPI ----

    def openapi(self):
        """Operation Object（辞書）。pydantic のスキーマは ``x-veil-components`` に載せる。"""
        func = self.func
        doc = inspect.getdoc(func) or ""
        lines = doc.strip().splitlines()
        summary = self.summary or (lines[0].strip() if lines else func.__name__.replace("_", " ").title())
        op = {"operationId": func.__name__, "summary": summary}
        if self.summary and doc:
            op["description"] = doc
        elif len(lines) > 1:
            op["description"] = "\n".join(lines[1:]).strip()
        if self.tags:
            op["tags"] = self.tags
        comps = {}
        params = []
        for p in self.params:
            if p.kind in ("path", "query"):
                schema = _scalar_schema(p.ann)
                if p.default is not _EMPTY and p.default is not None and not isinstance(p.default, (dict, list)):
                    schema["default"] = p.default
                params.append(
                    {
                        "name": p.name,
                        "in": p.kind,
                        "required": p.kind == "path" or p.required,
                        "schema": schema,
                    }
                )
        if params:
            op["parameters"] = params
        body = next((p for p in self.params if p.kind == "body"), None)
        schema = None
        required = True
        if body is not None and body.model is not None:
            schema = self._model_ref(body.model, comps)
            required = body.required
        elif self.body_constraints:
            schema = constraints_schema(self.body_constraints)
        elif body is not None:
            schema = {"type": "array"} if (body.ann is list or typing.get_origin(body.ann) is list) else {"type": "object"}
            required = body.required
        if schema is not None:
            op["requestBody"] = {"required": required, "content": {_JSON_CT: {"schema": schema}}}
        ok = {"description": "Successful Response"}
        ret = self._hints.get("return")
        if ret is not None and _is_model(ret):
            ok["content"] = {_JSON_CT: {"schema": self._model_ref(ret, comps)}}
        else:
            ok["content"] = {_JSON_CT: {"schema": {}}}
        responses = {str(self.status_code): ok}
        if params or schema is not None:
            responses["422"] = {
                "description": "Validation Error",
                "content": {_JSON_CT: {"schema": {"$ref": "#/components/schemas/HTTPValidationError"}}},
            }
        op["responses"] = responses
        if comps:
            op["x-veil-components"] = comps
        return op

    @staticmethod
    def _model_ref(model, comps):
        schema = model.model_json_schema(ref_template="#/components/schemas/{model}")
        defs = schema.pop("$defs", {})
        comps.update(defs)
        comps[model.__name__] = schema
        return {"$ref": "#/components/schemas/" + model.__name__}

    def openapi_json(self):
        return json.dumps(self.openapi(), separators=(",", ":"), ensure_ascii=False)


# --------------------------------------------------------------------------
# App
# --------------------------------------------------------------------------

_HANDLERS = []  # handler_id → Route（プロセス全体）


def _dispatch(handler_id, method, path, query, headers, body, params, client_ip):
    """Rust から GIL 下で呼ばれる。``(status, headers, body)`` を返す。"""
    route = _HANDLERS[handler_id]
    req = Request(method, path, query, headers, body, params, client_ip)
    return route.handle(req)._triple()


def _toml_literal(v):
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, (int, float)):
        return repr(v)
    if isinstance(v, str):
        return json.dumps(v)
    if isinstance(v, (list, tuple)):
        return "[" + ", ".join(_toml_literal(x) for x in v) + "]"
    if isinstance(v, dict):
        return "{" + ", ".join("%s = %s" % (json.dumps(str(k)), _toml_literal(x)) for k, x in v.items()) + "}"
    raise TypeError("TOML に変換できない値: %r" % (v,))


class App:
    """ルートの集合。``@app.get(...)`` で登録し、``app.run()`` で起動する。"""

    def __init__(self, title="veil API", version="0.1.0"):
        self.title = title
        self.version = version
        self.routes = []

    def route(self, path, methods=None, offload=True, body_constraints=None, tags=None, summary=None, status_code=200):
        def deco(func):
            self.routes.append(
                Route(func, path, methods or ["GET"], offload, body_constraints, tags, summary, status_code)
            )
            return func

        return deco

    def _verb(self, verb):
        def deco(path, **kw):
            kw.pop("methods", None)
            return self.route(path, methods=[verb], **kw)

        return deco

    def get(self, path, **kw):
        return self._verb("GET")(path, **kw)

    def post(self, path, **kw):
        return self._verb("POST")(path, **kw)

    def put(self, path, **kw):
        return self._verb("PUT")(path, **kw)

    def delete(self, path, **kw):
        return self._verb("DELETE")(path, **kw)

    def patch(self, path, **kw):
        return self._verb("PATCH")(path, **kw)

    def build_config(self, config_path=None, *, listen=None, threads=None, overrides=None, openapi=True):
        from . import _veil

        cfg = _veil.Config.from_file(config_path) if config_path else _veil.Config()
        if self.title != "veil API":
            cfg.set("api.openapi_title", _toml_literal(self.title))
        if self.version != "0.1.0":
            cfg.set("api.openapi_version", _toml_literal(self.version))
        if not openapi:
            cfg.set("api.openapi_enabled", "false")
        if listen:
            cfg.listen(listen)
        if threads:
            cfg.threads(int(threads))
        for k, v in (overrides or {}).items():
            cfg.set(k, _toml_literal(v))
        return cfg

    def run(self, config_path=None, *, listen=None, threads=None, overrides=None, openapi=True):
        """サーバを起動し、停止（Ctrl-C・SIGTERM・``veil.shutdown()``）までブロックする。"""
        from . import _veil

        cfg = self.build_config(config_path, listen=listen, threads=threads, overrides=overrides, openapi=openapi)
        for r in self.routes:
            hid = len(_HANDLERS)
            _HANDLERS.append(r)
            _veil.register_route(r.path, r.methods, hid, r.offload, r.openapi_json())
        _veil.run(cfg, _dispatch)


# モジュールレベルの既定 App（``@veil.get(...)`` / ``veil.run(...)``）
_default_app = App()
route = _default_app.route
get = _default_app.get
post = _default_app.post
put = _default_app.put
delete = _default_app.delete
patch = _default_app.patch
run = _default_app.run


def shutdown():
    """実行中のサーバを graceful に停止する。"""
    from . import _veil

    _veil.shutdown()


def __getattr__(name):
    # ``veil.Config`` はネイティブ拡張の遅延 import
    if name == "Config":
        from . import _veil

        return _veil.Config
    raise AttributeError(name)
