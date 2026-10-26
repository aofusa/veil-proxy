"""veil の Python の gRPC（F-210）。examples/api/rust-grpc と同じ demo.v1.Demo を実装する。

    # メッセージの生成（protobuf）と、grpcio の生成コード（grpcio の層を使うとき）
    python -m grpc_tools.protoc -I ../rust-grpc/proto --python_out=gen --grpc_python_out=gen ../rust-grpc/proto/demo.proto
    VEIL_GRPC_MODE=grpcio python server.py   # grpcio の生成コード（add_DemoServicer_to_server）をそのまま使う
    VEIL_GRPC_MODE=plain  python server.py   # 依存なしの基本の層（protobuf の FromString / SerializeToString）

    VEIL_LISTEN=127.0.0.1:50051（平文）、VEIL_TLS_LISTEN・VEIL_TLS_CERT・VEIL_TLS_KEY で TLS + HTTP/3 も。
"""

import os
import sys
import time

sys.path.insert(0, os.environ.get("GEN_DIR", os.path.join(os.path.dirname(os.path.abspath(__file__)), "gen")))

import veil  # noqa: E402
import veil.grpc  # noqa: E402
import demo_pb2 as pb  # noqa: E402


class Demo:
    def SayHello(self, request, context):
        caller = dict(context.invocation_metadata()).get("x-caller")
        if caller:
            context.send_initial_metadata([("x-caller-echo", caller)])
        context.set_trailing_metadata([("x-served-by", "veil-python")])
        return pb.HelloReply(message="Hello " + request.name)

    def CountUp(self, request, context):
        if request.count > 100000:
            context.abort(veil.grpc.StatusCode.INVALID_ARGUMENT, "count must be <= 100000")
        for n in range(1, request.count + 1):
            yield pb.CountReply(n=n)

    def Sum(self, request_iterator, context):
        total = count = 0
        for n in request_iterator:
            total += n.value
            count += 1
        return pb.SumReply(total=total, count=count)

    def Chat(self, request_iterator, context):
        for m in request_iterator:
            yield pb.ChatMessage(text=m.text.upper())

    def Sleep(self, request, context):
        end = time.time() + min(request.millis, 60000) / 1000
        while time.time() < end:
            left = context.time_remaining()
            if left is not None and left <= 0:
                context.abort(veil.grpc.StatusCode.DEADLINE_EXCEEDED, "deadline exceeded")
            time.sleep(0.01)
        return pb.Empty()

    def Fail(self, request, context):
        context.abort(request.code, request.message)


def register_plain(svc):
    """依存なしの基本の層（grpc を import しない）。"""
    g = veil.grpc
    p = "/demo.v1.Demo/"
    g.unary_unary(p + "SayHello", pb.HelloRequest.FromString, pb.HelloReply.SerializeToString)(svc.SayHello)
    g.unary_stream(p + "CountUp", pb.CountRequest.FromString, pb.CountReply.SerializeToString)(svc.CountUp)
    g.stream_unary(p + "Sum", pb.Number.FromString, pb.SumReply.SerializeToString)(svc.Sum)
    g.stream_stream(p + "Chat", pb.ChatMessage.FromString, pb.ChatMessage.SerializeToString)(svc.Chat)
    g.unary_unary(p + "Sleep", pb.SleepRequest.FromString, pb.Empty.SerializeToString)(svc.Sleep)
    g.unary_unary(p + "Fail", pb.FailRequest.FromString, pb.Empty.SerializeToString)(svc.Fail)


if __name__ == "__main__":
    mode = os.environ.get("VEIL_GRPC_MODE", "grpcio")
    if mode == "grpcio":
        import demo_pb2_grpc

        server = veil.grpc.Server()
        servicer = type("DemoServicer", (Demo, demo_pb2_grpc.DemoServicer), {})()
        demo_pb2_grpc.add_DemoServicer_to_server(servicer, server)
    else:
        register_plain(Demo())
    veil.grpc.add_health()
    kw = {"listen_cleartext": os.environ.get("VEIL_LISTEN", "127.0.0.1:50051"), "openapi": os.environ.get("VEIL_OPENAPI") == "1", "threads": int(os.environ.get("VEIL_THREADS", "0")) or None,
          "overrides": {"logging.level": os.environ.get("VEIL_LOG", "warn"), "api.grpc_response_compression": "gzip"}}
    tls = os.environ.get("VEIL_TLS_LISTEN")
    if tls:
        kw.update(listen=tls, tls_cert=os.environ["VEIL_TLS_CERT"], tls_key=os.environ["VEIL_TLS_KEY"])
        kw["overrides"].update({"http3.enabled": True, "http3.listen": tls})
    veil.run(**kw)
