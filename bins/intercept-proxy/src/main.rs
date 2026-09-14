//! `kloudlite-intercept-proxy`: the forwarder behind an intercepted environment service.

fn main() {
    tracing_subscriber::fmt().with_target(false).json().init();
    let args = match kloudlite_intercept_proxy::parse(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("kloudlite-intercept-proxy: {e}");
            std::process::exit(2);
        }
    };
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    // A bind failure exits non-zero; Kubernetes restarts the pod and the Service stays without a
    // ready endpoint, which is the honest answer. See `bind_all`.
    if let Err(e) = rt.block_on(kloudlite_intercept_proxy::serve(args)) {
        eprintln!("kloudlite-intercept-proxy: {e}");
        std::process::exit(1);
    }
}
