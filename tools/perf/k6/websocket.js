// WebSocket 計測: k6(WS) → veil(TLS 終端) → echo-server の WS エコー。
// 各セッションで MSGS 回のフレーム往復（送信→エコー受信）を行い、フレーム転送スループットを見る。
// 環境変数:
//   TARGET   veil の WS エンドポイント（wss://host:port/path、例 wss://veil-container:443/.ws）
//   VUS      並列仮想ユーザ数（既定 50）
//   DURATION 計測時間（既定 10s）
//   MSGS     1 セッションあたりのフレーム往復回数（既定 20）
// 出力: handleSummary が /out/result.tsv に "msgs_per_sec<TAB>connect_avg_ms<TAB>fails" を書く。
import ws from 'k6/ws';
import { check } from 'k6';

export const options = {
    insecureSkipTLSVerify: true,
    scenarios: {
        ws_echo: {
            executor: 'constant-vus',
            vus: Number(__ENV.VUS || 50),
            duration: __ENV.DURATION || '10s',
        },
    },
};

export default function () {
    const msgs = Number(__ENV.MSGS || 20);
    const res = ws.connect(__ENV.TARGET, {}, function (socket) {
        let received = 0;
        socket.on('open', function () {
            socket.send('ping');
        });
        socket.on('message', function () {
            received++;
            if (received >= msgs) {
                socket.close();
            } else {
                socket.send('ping');
            }
        });
        // ハングした接続を計測から締め出すセーフティタイムアウト。
        socket.setTimeout(function () {
            socket.close();
        }, 10000);
    });
    check(res, { 'status 101': (r) => r && r.status === 101 });
}

export function handleSummary(data) {
    const m = data.metrics;
    const rate = m.ws_msgs_received && m.ws_msgs_received.values ? m.ws_msgs_received.values.rate : 0;
    const conn = m.ws_connecting && m.ws_connecting.values ? m.ws_connecting.values.avg : 0;
    const fails = m.checks && m.checks.values ? m.checks.values.fails : 0;
    const line = [rate.toFixed(2), conn.toFixed(3), fails].join('\t') + '\n';
    return { '/out/result.tsv': line };
}
