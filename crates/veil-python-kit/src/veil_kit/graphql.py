"""GraphQL with strawberry (``pip install "veil-api-kit[graphql]"``).

.. code-block:: python

    import strawberry
    from veil_kit.graphql import mount

    schema = strawberry.Schema(query=Query, mutation=Mutation, subscription=Subscription)
    mount(app, "/graphql", schema)

One path serves ``POST`` (``Content-Type: application/json``, batches too; another type gives 415) and ``GET``
(query string; queries and subscriptions only: a mutation gives 405 so that a cross-site link cannot run one),
GraphQL over SSE for subscriptions (``Accept: text/event-stream``), WebSocket subscriptions
(``graphql-transport-ws`` and the older ``graphql-ws``) and GraphiQL for a browser ``GET`` (``graphiql=False`` turns
it off; the page loads its scripts from a CDN). A browser sends cookies with a cross-site WebSocket handshake and the
page can read the results, so a handshake with an ``Origin`` is accepted only from the same host as ``Host`` or from
``origins`` (others are rejected with 403).
``context_getter(request)`` builds the context passed to resolvers (default: ``{"request": request}``).
"""

# F-268: examples/api/python-graphql（F-210）を取り込み、WebSocket（F-263）のサブスクリプションを足した

import asyncio
import json

import veil

__all__ = ["mount", "GraphQLRoutes"]

_GRAPHQL_RESPONSE = "application/graphql-response+json"
_JSON_TYPES = ("application/json", _GRAPHQL_RESPONSE, "application/graphql+json")


def _is_json(content_type):
    # B-449: JSON 以外（text/plain のフォーム等、CORS の事前確認の要らない要求）は受けない（Rust の kit の B-196 と同じ）
    media = (content_type or "").split(";", 1)[0].strip().lower()
    return media in _JSON_TYPES


def _operation_type(query, operation_name):
    """The type of the operation a request would run (``"query"``, ``"mutation"`` or ``"subscription"``), or
    ``None`` if the document does not parse or does not select one operation."""
    from graphql import GraphQLError, OperationDefinitionNode, parse  # noqa: PLC0415 (graphql-core comes with strawberry)

    try:
        doc = parse(query)
    except GraphQLError:
        return None
    ops = [d for d in doc.definitions if isinstance(d, OperationDefinitionNode)]
    if operation_name:
        ops = [o for o in ops if o.name is not None and o.name.value == operation_name]
    if len(ops) != 1:
        return None
    return ops[0].operation.value


def _ws_origin_allowed(origin, host, allowed):
    """Whether a WebSocket handshake may use the endpoint: no ``Origin`` (not a browser), an ``Origin`` with the same
    host and port as ``Host``, or one of ``allowed``."""
    # B-447: ブラウザは別のサイトの WebSocket のハンドシェイクにも Cookie を付け、ページは結果を読めるので、Origin を確かめる
    if origin is None:
        return True
    if any(a.lower() == origin.lower() for a in allowed):
        return True
    scheme, sep, authority = origin.partition("://")
    default_port = {"https": ":443", "http": ":80"}.get(scheme.lower())
    if not host or not sep or not authority or default_port is None:
        return False

    def bare(a):
        a = a.strip().lower()
        return a[: -len(default_port)] if a.endswith(default_port) else a

    return bare(authority) == bare(host)


def _result(r):
    out = {"data": r.data}
    if r.errors:
        out["errors"] = [e.formatted if hasattr(e, "formatted") else {"message": str(e)} for e in r.errors]
    if getattr(r, "extensions", None):
        out["extensions"] = r.extensions
    return out


def _graphiql_html(path):
    try:
        from strawberry.http.ide import get_graphql_ide_html  # noqa: PLC0415

        return get_graphql_ide_html(graphql_ide="graphiql")
    except ImportError:
        pass
    try:
        from strawberry.utils.graphiql import get_graphiql_html  # noqa: PLC0415

        return get_graphiql_html()
    except ImportError:
        pass
    return (
        "<!doctype html><html><head><title>GraphiQL</title>"
        '<link rel="stylesheet" href="https://unpkg.com/graphiql@3/graphiql.min.css"></head>'
        '<body style="margin:0"><div id="g" style="height:100vh"></div>'
        '<script src="https://unpkg.com/react@18/umd/react.production.min.js"></script>'
        '<script src="https://unpkg.com/react-dom@18/umd/react-dom.production.min.js"></script>'
        '<script src="https://unpkg.com/graphiql@3/graphiql.min.js"></script>'
        "<script>ReactDOM.createRoot(document.getElementById('g')).render(React.createElement(GraphiQL,"
        "{fetcher:GraphiQL.createFetcher({url:%s})}))</script></body></html>" % json.dumps(path)
    )


class GraphQLRoutes:
    """The routes of one schema (created by :func:`mount`)."""

    def __init__(self, schema, path, graphiql=True, context_getter=None, keep_alive=None, origins=()):
        self.schema = schema
        self.path = path
        self.graphiql = graphiql
        self.context_getter = context_getter or (lambda request: {"request": request})
        self.keep_alive = keep_alive
        self.origins = tuple(origins)

    async def _execute(self, body, request):
        ctx = self.context_getter(request)
        if isinstance(ctx, asyncio.Future) or asyncio.iscoroutine(ctx):
            ctx = await ctx
        r = await self.schema.execute(body.get("query") or "", variable_values=body.get("variables"),
                                      operation_name=body.get("operationName"), context_value=ctx)
        return _result(r)

    async def _subscribe(self, body, request):
        ctx = self.context_getter(request)
        if asyncio.iscoroutine(ctx):
            ctx = await ctx
        r = await self.schema.subscribe(body.get("query") or "", variable_values=body.get("variables"),
                                        operation_name=body.get("operationName"), context_value=ctx)
        if hasattr(r, "__aiter__"):
            async for x in r:
                yield _result(x)
        else:
            yield _result(r)

    async def http(self, request: veil.Request):
        """``GET`` / ``POST`` on the GraphQL path."""
        accept = request.headers.get("accept") or ""
        if request.method in ("GET", "HEAD"):
            q = request.query_params
            if not q.get("query"):
                if self.graphiql and "text/html" in accept:
                    return veil.HTMLResponse(_graphiql_html(self.path))
                raise veil.HTTPException(400, "missing query")
            try:
                variables = json.loads(q["variables"]) if q.get("variables") else None
            except ValueError:
                raise veil.HTTPException(400, "variables is not valid JSON")
            body = {"query": q.get("query"), "variables": variables, "operationName": q.get("operationName")}
            # B-449: GET では mutation を実行しない（Lax の Cookie はトップレベルの GET 遷移に付く。Rust の kit の B-196）。
            # 解析できない文書も実行しない
            op = _operation_type(body["query"], body["operationName"])
            if op is None:
                raise veil.HTTPException(400, "the query does not parse or does not select one operation")
            if op == "mutation":
                raise veil.HTTPException(405, "mutations require POST", headers={"allow": "POST"})
        elif request.method == "POST":
            if not _is_json(request.headers.get("content-type")):
                raise veil.HTTPException(415, "GraphQL requests must be application/json")
            try:
                body = json.loads(request.body or b"null")
            except ValueError:
                raise veil.HTTPException(400, "the body is not valid JSON")
        else:
            raise veil.HTTPException(405, headers={"allow": "GET, POST"})
        if "text/event-stream" in accept:
            if not isinstance(body, dict):
                raise veil.HTTPException(400, "subscriptions over SSE take one operation")

            async def events():
                async for r in self._subscribe(body, request):
                    yield {"event": "next", "data": r}
                yield {"event": "complete", "data": ""}

            return veil.EventSourceResponse(events())
        if isinstance(body, list):
            out = [await self._execute(b, request) for b in body if isinstance(b, dict)]
        elif isinstance(body, dict):
            out = await self._execute(body, request)
        else:
            raise veil.HTTPException(400, "a GraphQL request is a JSON object")
        media = _GRAPHQL_RESPONSE if _GRAPHQL_RESPONSE in accept else "application/json"
        return veil.Response(json.dumps(out, separators=(",", ":")), 200, media_type=media)

    async def websocket(self, websocket: veil.WebSocket):
        """Subscriptions over WebSocket (``graphql-transport-ws`` and ``graphql-ws``)."""
        if not _ws_origin_allowed(websocket.headers.get("origin"), websocket.headers.get("host"), self.origins):
            await websocket.close(1008)
            return
        offered = websocket.subprotocols
        proto = next((p for p in ("graphql-transport-ws", "graphql-ws") if p in offered), None)
        if proto is None:
            await websocket.close(4406)
            return
        await websocket.accept(subprotocol=proto)
        legacy = proto == "graphql-ws"
        ops = {}
        acked = False

        async def run(op_id, payload):
            try:
                async for r in self._subscribe(payload, websocket):
                    await websocket.send_json({"type": "data" if legacy else "next", "id": op_id, "payload": r})
                await websocket.send_json({"type": "complete", "id": op_id})
            except asyncio.CancelledError:
                raise
            except Exception as e:  # noqa: BLE001
                await websocket.send_json({"type": "error", "id": op_id, "payload": [{"message": str(e)}]})
            finally:
                ops.pop(op_id, None)

        try:
            while True:
                try:
                    msg = await websocket.receive_json()
                except veil.WebSocketDisconnect:
                    break
                except (ValueError, TypeError):
                    await websocket.close(4400, "invalid message")
                    break
                t = msg.get("type") if isinstance(msg, dict) else None
                if t == "connection_init":
                    if acked and not legacy:
                        await websocket.close(4429, "Too many initialisation requests")
                        break
                    acked = True
                    await websocket.send_json({"type": "connection_ack"})
                elif t == "ping":
                    await websocket.send_json({"type": "pong", "payload": msg.get("payload")})
                elif t == "pong":
                    pass
                elif t in ("subscribe", "start"):
                    if not acked:
                        await websocket.close(4401, "Unauthorized")
                        break
                    op_id = msg.get("id")
                    if op_id in ops:
                        await websocket.close(4409, "Subscriber for %s already exists" % op_id)
                        break
                    ops[op_id] = asyncio.get_running_loop().create_task(run(op_id, msg.get("payload") or {}))
                elif t in ("complete", "stop"):
                    task = ops.pop(msg.get("id"), None)
                    if task is not None:
                        task.cancel()
                elif t == "connection_terminate":
                    await websocket.close(1000)
                    break
                else:
                    await websocket.close(4400, "Unknown message type")
                    break
        finally:
            for task in list(ops.values()):
                task.cancel()


def mount(app, path, schema, *, graphiql=True, context_getter=None, websocket=True, origins=()):
    """Serve ``schema`` (a ``strawberry.Schema``) at ``path`` on ``app``; returns the :class:`GraphQLRoutes`.

    ``origins`` lists other origins (such as ``"https://app.example"``) whose WebSocket handshakes are accepted.
    """
    routes = GraphQLRoutes(schema, path, graphiql, context_getter, origins=origins)

    async def graphql_http(request: veil.Request):
        return await routes.http(request)

    graphql_http.__name__ = "graphql" + path.replace("/", "_").replace("{", "").replace("}", "")
    app.route(path, methods=["GET", "POST"], tags=["graphql"], summary="GraphQL endpoint")(graphql_http)
    if websocket:
        async def graphql_ws(websocket: veil.WebSocket):
            await routes.websocket(websocket)

        graphql_ws.__name__ = graphql_http.__name__ + "_ws"
        # 同じパスの GET と WebSocket は、サーバが Upgrade の有無で振り分ける
        app.websocket(path, subprotocols=["graphql-transport-ws", "graphql-ws"], include_in_schema=False)(graphql_ws)
    return routes
