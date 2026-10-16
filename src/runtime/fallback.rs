//! io_uring が使えないときに、同じイメージに入れた readiness reactor（epoll）版の veil へ切り替える
//! （F-237・F-227）。
//!
//! Docker の既定の seccomp と Kubernetes の `RuntimeDefault` は `io_uring_setup` を拒否する。公開イメージは
//! io_uring 版（`/veil`）と epoll 版（`/veil.epoll`）の 2 つを持ち、io_uring 版は起動の最初にここで
//! io_uring を作れるかを確かめ、作れなければ epoll 版を同じ引数・環境変数で `execve(2)` する。
//! 確かめるのは起動時の 1 回だけで、要求の経路には関係しない。
//!
//! 切り替え先は `VEIL_FALLBACK_BIN`（あれば）か、自分の実行ファイルのパス + `.epoll`。どちらも無ければ
//! 何もしない（呼び出し側の従来の扱いのまま）。切り替えた先で同じ確認が走り続けないよう、
//! `VEIL_FALLBACK_EXECED` を付けて exec する（epoll 版はそもそもこの確認を持たない）。

use std::ffi::OsString;
use std::path::PathBuf;

/// 切り替え先を上書きする環境変数。
pub const FALLBACK_BIN_ENV: &str = "VEIL_FALLBACK_BIN";
/// 切り替えた後であることを示す環境変数（再帰の防止）。
const EXECED_ENV: &str = "VEIL_FALLBACK_EXECED";

/// io_uring を作れなければ切り替え先を exec する。exec できたら戻らない。
///
/// 戻るのは次のどれか: io_uring が使える・切り替え先が無い・既に切り替えた後・exec に失敗した。
pub fn exec_reactor_if_uring_unavailable() {
    let err = match super::ring::IoUring::new(8, 0) {
        Ok(_) => return,
        Err(e) => e,
    };
    if std::env::var_os(EXECED_ENV).is_some() {
        return;
    }
    let Some(target) = fallback_path(
        std::env::var_os(FALLBACK_BIN_ENV),
        std::fs::read_link("/proc/self/exe").ok(),
    ) else {
        return;
    };
    if !target.is_file() {
        return;
    }
    // ロガーはまだ初期化していない（設定を読む前）ので標準エラーへ 1 回だけ出す
    eprintln!(
        "veil: io_uring is not available ({err}); the container's seccomp profile probably forbids it. \
         Switching to the epoll build {}",
        target.display()
    );
    use std::os::unix::process::CommandExt;
    let mut args = std::env::args_os();
    let argv0 = args.next().unwrap_or_else(|| OsString::from("veil"));
    let e = std::process::Command::new(&target)
        .arg0(argv0)
        .args(args)
        .env(EXECED_ENV, "1")
        .exec();
    eprintln!("veil: cannot exec {}: {e}", target.display());
}

/// 切り替え先のパス: `VEIL_FALLBACK_BIN` が優先、無ければ自分のパス + `.epoll`。
fn fallback_path(env: Option<OsString>, self_exe: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = env.filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let exe = self_exe?;
    let mut name = exe.file_name()?.to_os_string();
    name.push(".epoll");
    Some(exe.with_file_name(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_overrides_the_default_path() {
        let p = fallback_path(Some("/opt/veil-epoll".into()), Some("/veil".into()));
        assert_eq!(p, Some(PathBuf::from("/opt/veil-epoll")));
    }

    #[test]
    fn default_is_self_path_with_epoll_suffix() {
        assert_eq!(
            fallback_path(None, Some("/usr/local/bin/envoy".into())),
            Some(PathBuf::from("/usr/local/bin/envoy.epoll"))
        );
        // 空の環境変数は無いのと同じ
        assert_eq!(
            fallback_path(Some(OsString::new()), Some("/veil".into())),
            Some(PathBuf::from("/veil.epoll"))
        );
    }

    #[test]
    fn no_self_path_means_no_fallback() {
        assert_eq!(fallback_path(None, None), None);
    }
}
