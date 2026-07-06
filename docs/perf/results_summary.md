| Target | Config | Proto | Req/s (Median ± Stdev) | Latency Avg (Median) | CPU% (Median) | Mem MB (Median) | Errors |
|---|---|---|---|---|---|---|---|
| nginx | base | http1.1 | 1994.5 ± 81.9 | 47.37ms | 199.9% | 22.0MB | 0 |
| nginx | base | http2 | 2275.8 ± 38.1 | 226.78ms | 154.7% | 25.5MB | 0 |
| veil_glibc | h2_1_feat_buffering | http1.1 | 1574.8 ± 25.3 | 60.86ms | 159.8% | 92.4MB | 0 |
| veil_glibc | h2_1_feat_buffering | http2 | 741.9 ± 155.3 | 169.28ms | 95.8% | 122.4MB | 0 |
| veil_glibc | h2_1_feat_cache | http1.1 | 1822.2 ± 23.8 | 53.25ms | 180.0% | 66.3MB | 0 |
| veil_glibc | h2_1_feat_cache | http2 | 772.4 ± 231.2 | 77.36ms | 34.2% | 95.1MB | 0 |
| veil_glibc | h2_1_feat_compression | http1.1 | 1817.1 ± 37.2 | 53.40ms | 181.7% | 82.9MB | 0 |
| veil_glibc | h2_1_feat_compression | http2 | 571.0 ± 84.4 | 103.63ms | 25.0% | 123.3MB | 0 |
| veil_glibc | h2_1_feat_proxy | http1.1 | 0.0 ± 0.0 | 0.00ms | 11.3% | 75.5MB | 0 |
| veil_glibc | h2_1_feat_proxy | http2 | 718.3 ± 40.9 | 146.02ms | 94.2% | 109.5MB | 0 |
| veil_glibc | h2_1_ktls_0_lb_cbpf_ofc_0 | http1.1 | 1378.9 ± 5.4 | 69.40ms | 89.8% | 66.5MB | 0 |
| veil_glibc | h2_1_ktls_0_lb_cbpf_ofc_0 | http2 | 2226.5 ± 34.7 | 299.34ms | 67.8% | 99.7MB | 0 |
| veil_glibc | h2_1_ktls_1_lb_kernel_ofc_1 | http1.1 | 2132.2 ± 26.3 | 45.50ms | 176.5% | 92.0MB | 0 |
| veil_glibc | h2_1_ktls_1_lb_kernel_ofc_1 | http2 | 250.8 ± 115.3 | 256.49ms | 15.8% | 124.2MB | 0 |
| veil_musl | h2_1_feat_buffering | http1.1 | 1505.8 ± 31.8 | 64.06ms | 155.8% | 118.0MB | 0 |
| veil_musl | h2_1_feat_buffering | http2 | 555.5 ± 231.5 | 182.03ms | 97.3% | 138.4MB | 0 |
| veil_musl | h2_1_feat_cache | http1.1 | 1818.2 ± 68.0 | 52.39ms | 174.2% | 86.7MB | 0 |
| veil_musl | h2_1_feat_cache | http2 | 270.1 ± 284.7 | 261.25ms | 26.8% | 121.0MB | 0 |
| veil_musl | h2_1_feat_compression | http1.1 | 1852.8 ± 23.4 | 52.31ms | 167.4% | 82.7MB | 0 |
| veil_musl | h2_1_feat_compression | http2 | 591.5 ± 191.5 | 109.10ms | 22.7% | 124.9MB | 0 |
| veil_musl | h2_1_feat_proxy | http1.1 | 0.0 ± 0.0 | 0.00ms | 11.6% | 74.4MB | 0 |
| veil_musl | h2_1_feat_proxy | http2 | 863.4 ± 357.2 | 154.01ms | 99.5% | 107.6MB | 0 |
| veil_musl | h2_1_ktls_0_lb_cbpf_ofc_0 | http1.1 | 1390.4 ± 14.6 | 68.97ms | 88.5% | 68.4MB | 0 |
| veil_musl | h2_1_ktls_0_lb_cbpf_ofc_0 | http2 | 2393.7 ± 104.0 | 277.17ms | 56.2% | 102.4MB | 0 |
| veil_musl | h2_1_ktls_1_lb_kernel_ofc_1 | http1.1 | 2158.8 ± 19.3 | 44.45ms | 154.3% | 93.6MB | 0 |
| veil_musl | h2_1_ktls_1_lb_kernel_ofc_1 | http2 | 258.2 ± 101.0 | 263.27ms | 15.5% | 131.2MB | 0 |
