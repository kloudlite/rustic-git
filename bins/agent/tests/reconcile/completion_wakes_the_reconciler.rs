//! completion wakes the reconciler.

use super::*;


/// Take one wake-up, or fail well before the 15s requeue that used to be the only path.
pub(crate) async fn wake<T>(rx: &mut tokio::sync::mpsc::UnboundedReceiver<T>) -> T {
    tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("no wake-up before the timeout: the object would have waited out TICK")
        .expect("the wake channel closed")
}

/// A finished volume operation sends its own ref, so the reconcile that writes `ready` happens on
/// completion rather than on the 15s tick.
#[tokio::test]
async fn a_finished_volume_operation_wakes_its_reconciler() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![patch_ok(VOL_STATUS)]);
    let (mut vol_wakes, _ws_wakes) = ctx.wakes.lock().unwrap().take().unwrap();
    let v = volume(3);

    let action = kloudlite_agent::controller::apply_volume(&v, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)));

    assert_eq!(wake(&mut vol_wakes).await.name, "vol-1");

    // The woken pass observes the handle and writes the outcome. There is no btrfs on the test
    // host, so that outcome is `error` — the `ready` half is `a_finished_operation_writes_observed_
    // generation_and_stops_requeueing`; what is under test here is that the pass happens at all.
    wait_idle(&ctx).await;
    kloudlite_agent::controller::apply_volume(&v, &ctx).await.unwrap();
    let sent = rec.sent("PATCH", VOL_STATUS);
    let last = sent.last().unwrap();
    assert_ne!(last["status"]["phase"], "working", "the wake-up pass must leave `working`: {last}");
    assert!(ctx.running.lock().unwrap().is_empty(), "the finished handle must be drained");
}


/// The success half: a finished operation whose outcome is `Ready` wakes the reconciler AND the
/// woken pass writes `ready` with the generation it ran for. Stubbed through `wake_on_finish`
/// rather than through real work, because the test host has no btrfs.
#[tokio::test]
async fn a_successful_volume_operation_wakes_and_then_writes_ready() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![patch_ok(VOL_STATUS)]);
    let (mut vol_wakes, _ws_wakes) = ctx.wakes.lock().unwrap().take().unwrap();
    let v = volume(5);
    let handle = kloudlite_agent::controller::wake_on_finish(
        tokio::task::spawn_blocking(|| Ok(Done { phase: crd::Phase::Ready, ..Done::default() })),
        ctx.wake_volume.clone(),
        kube::runtime::reflector::ObjectRef::<crd::Volume>::new("vol-1"),
    );
    ctx.running.lock().unwrap().insert("uid-1".to_string(), (5, handle));

    assert_eq!(wake(&mut vol_wakes).await.name, "vol-1");

    wait_idle(&ctx).await;
    let action = kloudlite_agent::controller::apply_volume(&v, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change());
    let sent = rec.sent("PATCH", VOL_STATUS);
    let last = sent.last().unwrap();
    assert_eq!(last["status"]["phase"], "ready", "{last}");
    assert_eq!(last["status"]["observedGeneration"], 5);
    assert!(ctx.running.lock().unwrap().is_empty(), "the finished handle must be drained");
}
