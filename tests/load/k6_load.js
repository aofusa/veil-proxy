// F-56: k6 負荷スクリプト（閾値付き）。
//
// error 率と p95 latency に合否閾値を設定し、chaos 併用時の劣化を自動判定する。
// 環境変数で調整（run_load.sh が設定）:
//   TARGET_URL       負荷対象 URL
//   DURATION         継続時間（例: "20s"）
//   CONNECTIONS      同時 VU 数
//   MAX_ERROR_RATE   許容エラー率（0.05 = 5%）
//   MAX_P95_MS       許容 p95 latency（ミリ秒）
import http from 'k6/http';
import { check } from 'k6';
import { Rate } from 'k6/metrics';

const errorRate = new Rate('errors');

const TARGET_URL = __ENV.TARGET_URL || 'https://127.0.0.1:443/';
const DURATION = __ENV.DURATION || '20s';
const CONNECTIONS = parseInt(__ENV.CONNECTIONS || '200', 10);
const MAX_ERROR_RATE = parseFloat(__ENV.MAX_ERROR_RATE || '0.05');
const MAX_P95_MS = parseInt(__ENV.MAX_P95_MS || '1000', 10);

export const options = {
  vus: CONNECTIONS,
  duration: DURATION,
  // 自己署名証明書のテスト環境向け（本番計測では外すこと）。
  insecureSkipTLSVerify: true,
  thresholds: {
    errors: [`rate<${MAX_ERROR_RATE}`],
    http_req_duration: [`p(95)<${MAX_P95_MS}`],
  },
};

export default function () {
  const res = http.get(TARGET_URL);
  const ok = check(res, { 'status is 2xx/3xx': (r) => r.status >= 200 && r.status < 400 });
  errorRate.add(!ok);
}
