"""A gRPC client with ``veil_kit.grpc`` and the stubs of ``grpc_tools.protoc`` (F-268).

    python -m grpc_tools.protoc -I ../rust-grpc/proto --python_out=gen --grpc_python_out=gen ../rust-grpc/proto/demo.proto
    GEN_DIR=gen python grpc_client.py 127.0.0.1:50051 [--tls 127.0.0.1:50443 cert.pem]

Calls every method of ``demo.v1.Demo`` (examples/api/python-grpc/server.py or examples/api/rust-grpc) with the
blocking channel and with the asyncio channel, and prints ``ok <name>`` or ``FAIL <name>`` per check.
"""

import asyncio
import os
import sys

sys.path.insert(0, os.environ.get("GEN_DIR", os.path.join(os.path.dirname(os.path.abspath(__file__)), "gen")))

import demo_pb2  # noqa: E402
import demo_pb2_grpc  # noqa: E402

from veil_kit import grpc as vgrpc  # noqa: E402

failed = False


def check(name, got, want):
    global failed
    if got == want:
        print("ok", name)
    else:
        failed = True
        print("FAIL %s: got %r, want %r" % (name, got, want))


def blocking(channel, label):
    stub = demo_pb2_grpc.DemoStub(channel)
    check(label + " unary", stub.SayHello(demo_pb2.HelloRequest(name="kit"), timeout=10).message, "Hello kit")
    check(label + " server streaming", [r.n for r in stub.CountUp(demo_pb2.CountRequest(count=3))], [1, 2, 3])
    s = stub.Sum(iter([demo_pb2.Number(value=v) for v in (1, 2, 3, 4)]))
    check(label + " client streaming", (s.total, s.count), (10, 4))
    msgs = [demo_pb2.ChatMessage(text=t) for t in ("a", "bc")]
    check(label + " bidi", [m.text for m in stub.Chat(iter(msgs))], ["A", "BC"])
    try:
        stub.Fail(demo_pb2.FailRequest(code=5, message="nope"))
        check(label + " error status", "no error", "NOT_FOUND")
    except vgrpc.RpcError as e:
        check(label + " error status", (e.code().name, e.details()), ("NOT_FOUND", "nope"))
    try:
        stub.Sleep(demo_pb2.SleepRequest(millis=3000), timeout=0.2)
        check(label + " deadline", "no error", "DEADLINE_EXCEEDED")
    except vgrpc.RpcError as e:
        check(label + " deadline", e.code().name, "DEADLINE_EXCEEDED")


async def asynchronous(target):
    async with vgrpc.aio.insecure_channel(target) as ch:
        stub = demo_pb2_grpc.DemoStub(ch)
        r = await stub.SayHello(demo_pb2.HelloRequest(name="aio"))
        check("aio unary", r.message, "Hello aio")
        check("aio server streaming", [r.n async for r in stub.CountUp(demo_pb2.CountRequest(count=2))], [1, 2])
        call = stub.Chat()
        await call.write(demo_pb2.ChatMessage(text="x"))
        first = await call.read()
        await call.done_writing()
        check("aio bidi", first.text, "X")


def main():
    target = sys.argv[1]
    with vgrpc.insecure_channel(target) as ch:
        blocking(ch, "h2c")
    if "--tls" in sys.argv:
        i = sys.argv.index("--tls")
        with open(sys.argv[i + 2], "rb") as f:
            creds = vgrpc.ssl_channel_credentials(root_certificates=f.read())
        with vgrpc.secure_channel(sys.argv[i + 1], creds) as ch:
            blocking(ch, "tls")
    asyncio.run(asynchronous(target))
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
