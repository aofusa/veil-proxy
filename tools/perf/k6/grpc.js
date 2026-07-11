// gRPC 計測: k6(gRPC) → veil(TLS 終端 h2) → grpcbin(h2c) の unary 呼び出し。
// 環境変数:
//   TARGET   veil の gRPC エンドポイント（host:port、例 veil-container:443）
//   VUS      並列仮想ユーザ数（既定 50）
//   DURATION 計測時間（既定 10s）
// 出力: handleSummary が /out/result.tsv に "reqps<TAB>lat_avg_ms<TAB>fails" を書く。
import grpc from 'k6/net/grpc';
import { check } from 'k6';

const client = new grpc.Client();
client.load(['/scripts'], 'hello.proto');

export const options = {
    // veil は自己署名証明書のため検証をスキップ（計測はハンドシェイク成功前提）。
    insecureSkipTLSVerify: true,
    scenarios: {
        grpc_unary: {
            executor: 'constant-vus',
            vus: Number(__ENV.VUS || 50),
            duration: __ENV.DURATION || '10s',
        },
    },
};

export default function () {
    // 各 VU は初回のみ接続を確立し、以降の反復で再利用する（接続確立コストを計測外にする）。
    if (__ITER === 0) {
        client.connect(__ENV.TARGET, { plaintext: false, timeout: '5s' });
    }
    const res = client.invoke('hello.HelloService/SayHello', { greeting: 'veil' });
    check(res, { 'status OK': (r) => r && r.status === grpc.StatusOK });
}

export function handleSummary(data) {
    const m = data.metrics;
    const reqps = m.iterations && m.iterations.values ? m.iterations.values.rate : 0;
    const lat = m.grpc_req_duration && m.grpc_req_duration.values ? m.grpc_req_duration.values.avg : 0;
    const fails = m.checks && m.checks.values ? m.checks.values.fails : 0;
    const line = [reqps.toFixed(2), lat.toFixed(3), fails].join('\t') + '\n';
    return { '/out/result.tsv': line };
}
