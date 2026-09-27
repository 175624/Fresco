//! Test-only harness for the fake-daemon integration test
//! (`tests/cinnamon_bg_fake_daemon.sh`, invoked via the `#[ignore]`d
//! `daemon::cinnamon_bg::tests::restack_against_fake_daemon`).
//!
//! Not part of the product; it exercises the real `cinnamon_bg::restack()`
//! and `ensure_daemon_running()` functions against whatever session bus is
//! active in the environment (the shell harness runs this under
//! `dbus-run-session` with a fake `org.Cinnamon.Background` service
//! advertised via `XDG_DATA_DIRS`).
//!
//! Usage: `cinnamon_restack_probe <ensure|restack>`

#[cfg(feature = "daemon")]
fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();
    let cmd = std::env::args().nth(1).unwrap_or_default();
    match cmd.as_str() {
        "ensure" => fresco::daemon::cinnamon_bg::ensure_daemon_running(),
        "restack" => fresco::daemon::cinnamon_bg::restack(),
        other => {
            eprintln!("usage: cinnamon_restack_probe <ensure|restack> (got {other:?})");
            std::process::exit(2);
        }
    }
}

#[cfg(not(feature = "daemon"))]
fn main() {
    eprintln!("cinnamon_restack_probe requires the `daemon` feature");
    std::process::exit(1);
}
