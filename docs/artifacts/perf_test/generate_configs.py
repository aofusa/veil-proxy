import re

with open("docker/assets/conf.d/config.toml", "r") as f:
    base = f.read()

with open("docs/artifacts/perf_test/configs/config_base.toml", "w") as f:
    f.write(base)

with open("docs/artifacts/perf_test/configs/config_no_ktls.toml", "w") as f:
    f.write(re.sub(r'ktls_enabled\s*=\s*true', 'ktls_enabled = false', base))

with open("docs/artifacts/perf_test/configs/config_no_http2.toml", "w") as f:
    f.write(re.sub(r'http2_enabled\s*=\s*true', 'http2_enabled = false', base))

with open("docs/artifacts/perf_test/configs/config_kernel_lb.toml", "w") as f:
    f.write(re.sub(r'reuseport_balancing\s*=\s*"cbpf"', 'reuseport_balancing = "kernel"', base))

with open("docs/artifacts/perf_test/configs/config_ofc.toml", "w") as f:
    f.write(re.sub(r'#?\s*open_file_cache_enabled\s*=\s*false', 'open_file_cache_enabled = true', base))

