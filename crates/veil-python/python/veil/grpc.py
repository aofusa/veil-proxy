"""veil の Python から gRPC のサーバを書く（F-210）。

2 つの層がある。どちらもメッセージの直列化は利用者が選んだライブラリ（protobuf・betterproto・JSON 等）が行い、
veil 自身は protobuf の実装に依存しない。

**基本の層（依存なし）**::

    import veil, veil.grpc
    from demo_pb2 import HelloRequest, HelloReply        # protoc --python_out で生成したもの等

    @veil.grpc.unary_unary("/demo.v1.Demo/SayHello",
                           request_deserializer=HelloRequest.FromString,
                           response_serializer=HelloReply.SerializeToString)
    def say_hello(request, context):
        return HelloReply(message="Hello " + request.name)

    veil.run(listen_cleartext="127.0.0.1:50051")

``request_deserializer`` / ``response_serializer`` を省くと bytes のまま受け渡す。``unary_stream``・``stream_unary``・
``stream_stream`` も同じ（ストリーミングの入力は反復子、出力は反復子・ジェネレータ）。``async def`` と非同期
ジェネレータも使える（ハンドラ用プールのスレッドごとのイベントループで動く）。

**grpcio の層（``pip install veil-api[grpc]``）**: ``grpc_tools.protoc`` が生成した ``*_pb2_grpc.py`` の
``add_<Service>Servicer_to_server(servicer, server)`` に :class:`Server` を渡せば、そのまま動く::

    server = veil.grpc.Server()
    demo_pb2_grpc.add_DemoServicer_to_server(MyServicer(), server)
    server.add_insecure_port("127.0.0.1:50051")
    server.start(); server.wait_for_termination()     # = veil.run(listen_cleartext=...)

``context`` は grpcio の ``ServicerContext`` の主要部分（``abort``・``set_code``・``set_details``・
``invocation_metadata``・``send_initial_metadata``・``set_trailing_metadata``・``time_remaining``・``is_active``・
``peer``）を持つ。
"""

import collections
import enum
import inspect
import traceback

from . import _default_app, _run_coro, _aiter_sync

__all__ = [
    "StatusCode",
    "ServicerContext",
    "Server",
    "unary_unary",
    "unary_stream",
    "stream_unary",
    "stream_stream",
    "add_health",
]


class StatusCode(enum.IntEnum):
    """gRPC のステータスコード（grpcio の ``grpc.StatusCode`` も受け付ける）。"""

    OK = 0
    CANCELLED = 1
    UNKNOWN = 2
    INVALID_ARGUMENT = 3
    DEADLINE_EXCEEDED = 4
    NOT_FOUND = 5
    ALREADY_EXISTS = 6
    PERMISSION_DENIED = 7
    RESOURCE_EXHAUSTED = 8
    FAILED_PRECONDITION = 9
    ABORTED = 10
    OUT_OF_RANGE = 11
    UNIMPLEMENTED = 12
    INTERNAL = 13
    UNAVAILABLE = 14
    DATA_LOSS = 15
    UNAUTHENTICATED = 16


def _code(c):
    """int・StatusCode・grpcio の grpc.StatusCode（value が (int, str)）を int にする。"""
    if c is None:
        return 0
    v = getattr(c, "value", c)
    if isinstance(v, tuple):
        v = v[0]
    return int(v)


class _Abort(Exception):
    def __init__(self, code, details):
        super().__init__(details)
        self.code = _code(code)
        self.details = details or ""


_Metadatum = collections.namedtuple("_Metadatum", ["key", "value"])


class ServicerContext:
    """1 回の呼び出しの文脈（grpcio の ``ServicerContext`` と同じ名前のメソッド）。"""

    def __init__(self, call):
        self._call = call
        self._code = None
        self._details = ""

    def abort(self, code, details=""):
        raise _Abort(code, details)

    def abort_with_status(self, status):
        raise _Abort(status.code, getattr(status, "details", ""))

    def set_code(self, code):
        self._code = code

    def set_details(self, details):
        self._details = details

    def code(self):
        return self._code

    def details(self):
        return self._details

    def invocation_metadata(self):
        return tuple(_Metadatum(k, v) for k, v in self._call.metadata())

    def send_initial_metadata(self, initial_metadata):
        for k, v in initial_metadata:
            self._call.add_header(k, v)

    def set_trailing_metadata(self, trailing_metadata):
        for k, v in trailing_metadata:
            self._call.add_trailer(k, v)

    def time_remaining(self):
        return self._call.time_remaining()

    def is_active(self):
        return self._call.is_active()

    def peer(self):
        return ""

    def method(self):
        return self._call.method()


class _RequestIterator:
    def __init__(self, call, des):
        self._call = call
        self._des = des

    def __iter__(self):
        return self

    def __next__(self):
        b = _recv(self._call)
        if b is None:
            raise StopIteration
        return self._des(b)


def _recv(call):
    try:
        return call.recv()
    except RuntimeError as e:
        if len(e.args) == 2 and isinstance(e.args[0], int):
            raise _Abort(e.args[0], e.args[1])
        raise


def _identity(x):
    return x


class _Method:
    """登録した 1 メソッド（Rust が ``handle(call) -> (code, message)`` を呼ぶ）。"""

    def __init__(self, behavior, request_streaming, response_streaming, deserializer, serializer):
        self.behavior = behavior
        self.request_streaming = request_streaming
        self.response_streaming = response_streaming
        self.des = deserializer or _identity
        self.ser = serializer or _identity

    def handle(self, call):
        ctx = ServicerContext(call)
        try:
            if self.request_streaming:
                request = _RequestIterator(call, self.des)
            else:
                first = _recv(call)
                if first is None:
                    return (13, "missing request message")
                if _recv(call) is not None:
                    return (13, "unary call received more than one request message")
                request = self.des(first)
            result = self.behavior(request, ctx)
            if inspect.iscoroutine(result):
                result = _run_coro(result)
            if self.response_streaming:
                it = _aiter_sync(result) if hasattr(result, "__aiter__") else iter(result)
                for m in it:
                    if not call.send(self.ser(m)):
                        close = getattr(it, "close", None)
                        if close:
                            close()
                        return (1, "cancelled")
            else:
                if result is None:
                    if _code(ctx._code) != 0:
                        return (_code(ctx._code), ctx._details)
                    return (13, "no response message")
                if _code(ctx._code) in (0,) or ctx._code is None:
                    call.send(self.ser(result))
            return (_code(ctx._code), ctx._details if _code(ctx._code) != 0 else "")
        except _Abort as a:
            return (a.code, a.details)
        except Exception as e:  # grpcio と同じく UNKNOWN
            traceback.print_exc()
            return (2, "Exception calling application: %s" % e)


def _register(app, path, behavior, req_s, resp_s, des, ser):
    app = app or _default_app
    app.grpc_methods.append((path, _Method(behavior, req_s, resp_s, des, ser)))


def _decorator(req_s, resp_s):
    def make(path, request_deserializer=None, response_serializer=None, app=None):
        def deco(func):
            _register(app, path, func, req_s, resp_s, request_deserializer, response_serializer)
            return func

        return deco

    return make


unary_unary = _decorator(False, False)
unary_stream = _decorator(False, True)
stream_unary = _decorator(True, False)
stream_stream = _decorator(True, True)
unary_unary.__doc__ = "単項（1 要求 → 1 応答）。``func(request, context) -> response``。"
unary_stream.__doc__ = "サーバストリーミング。``func(request, context)`` は応答の反復子・ジェネレータを返す。"
stream_unary.__doc__ = "クライアントストリーミング。``func(request_iterator, context) -> response``。"
stream_stream.__doc__ = "双方向（全二重）。``func(request_iterator, context)`` は応答の反復子を返す。"


def add_health(app=None):
    """``grpc.health.v1.Health``（Check・Watch）を登録する（サーバ全体は SERVING）。"""
    (app or _default_app).grpc_health = True


class Server:
    """grpcio の ``grpc.Server`` の代わりに生成コード（``add_<Service>Servicer_to_server``）へ渡すもの。

    メソッドは veil の ``App``（既定はモジュールの既定 App）に登録され、``start()`` / ``wait_for_termination()``
    （または ``app.run(...)``）で起動する。
    """

    def __init__(self, app=None):
        self.app = app or _default_app
        self._listen = None
        self._kw = {}

    def add_generic_rpc_handlers(self, generic_rpc_handlers):
        for h in generic_rpc_handlers:
            handlers = getattr(h, "_method_handlers", None)
            if handlers is None:
                raise TypeError(
                    "veil.grpc.Server supports handlers made by grpc.method_handlers_generic_handler "
                    "(the code generated by grpc_tools.protoc)"
                )
            for path, rpc in handlers.items():
                self._add(path, rpc)

    def add_registered_method_handlers(self, service_name, method_handlers):
        for name, rpc in method_handlers.items():
            self._add("/%s/%s" % (service_name, name), rpc)

    def _add(self, path, rpc):
        # 新しい grpcio-tools の生成コードは add_generic_rpc_handlers と add_registered_method_handlers の
        # 両方を呼ぶので、同じメソッドは 1 回だけ登録する
        if any(p == path for p, _ in self.app.grpc_methods):
            return
        req_s, resp_s = bool(rpc.request_streaming), bool(rpc.response_streaming)
        behavior = (
            rpc.stream_stream
            if req_s and resp_s
            else rpc.stream_unary if req_s else rpc.unary_stream if resp_s else rpc.unary_unary
        )
        _register(self.app, path, behavior, req_s, resp_s, rpc.request_deserializer, rpc.response_serializer)

    def add_insecure_port(self, address):
        """平文（h2c）で受けるアドレス（``[::]:50051`` も可）。"""
        self._listen = address.replace("[::]", "0.0.0.0")
        return int(address.rsplit(":", 1)[1])

    def add_secure_port(self, address, server_credentials=None):
        raise NotImplementedError(
            "use app.run(listen=..., tls_cert=..., tls_key=...) for TLS (HTTP/2 and HTTP/3)"
        )

    def start(self):
        pass

    def wait_for_termination(self, timeout=None, **kw):
        """サーバを起動して停止までブロックする（``veil.run`` と同じ）。"""
        kw = dict(self._kw, **kw)
        if self._listen and "listen_cleartext" not in kw:
            kw["listen_cleartext"] = self._listen
        self.app.run(**kw)

    def stop(self, grace=None):
        from . import shutdown

        shutdown()
