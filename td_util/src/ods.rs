/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! One-shot tagged ODS counters for short-lived target-determinator processes.

use std::time::Duration;
#[cfg(target_os = "linux")]
use std::time::SystemTime;

use anyhow::Context;
use async_trait::async_trait;
use fbinit::FacebookInit;
#[cfg(target_os = "linux")]
use maestro::AggSettings;
#[cfg(target_os = "linux")]
use maestro::ODSAppValueWithTags;
#[cfg(target_os = "linux")]
use ods_router::SetOdsRawValuesWithTagsRequest;
#[cfg(target_os = "linux")]
use ods_router_x2pclients::make_OdsRouter_x2pclient;
use tracing::warn;

#[cfg(target_os = "linux")]
const ODS_INTERVAL_SECONDS: i32 = 60;

#[derive(Clone, Debug, PartialEq)]
pub struct OdsPoint {
    pub key: String,
    pub value: f64,
    pub aggregate_entity: String,
}

#[async_trait]
pub trait OdsSender: Send + Sync {
    async fn send(
        &self,
        fb: FacebookInit,
        source_entity: &str,
        category: &str,
        category_id: i32,
        points: Vec<OdsPoint>,
    ) -> anyhow::Result<()>;
}

pub struct OdsSink {
    fb: FacebookInit,
    source_entity: String,
    category: String,
    category_id: i32,
    sender: Box<dyn OdsSender>,
}

impl OdsSink {
    /// Each caller gets a bounded source entity, while tags retain its desired
    /// fleet-level aggregate entity independently of raw storage.
    pub fn new(
        fb: FacebookInit,
        category: &str,
        category_id: i32,
        source_prefix: &str,
    ) -> anyhow::Result<Self> {
        let host = hostname::get().context("Failed to get hostname for ODS source")?;
        Ok(Self::with_sender(
            fb,
            category,
            category_id,
            format!("{source_prefix}.host.{}", host.to_string_lossy()),
            Box::new(RouterSender),
        ))
    }

    /// An injected sender and source make transport tests deterministic without
    /// sending real ODS datapoints.
    pub fn with_sender(
        fb: FacebookInit,
        category: &str,
        category_id: i32,
        source_entity: String,
        sender: Box<dyn OdsSender>,
    ) -> Self {
        Self {
            fb,
            source_entity,
            category: category.to_owned(),
            category_id,
            sender,
        }
    }

    /// ODS telemetry is alert-only: transport errors and timeouts cannot fail
    /// the caller's ranking or scheduling work.
    pub async fn send_tagged(&self, points: Vec<OdsPoint>, timeout: Duration) {
        if points.is_empty() {
            return;
        }
        match tokio::time::timeout(
            timeout,
            self.sender.send(
                self.fb,
                &self.source_entity,
                &self.category,
                self.category_id,
                points,
            ),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(err)) => warn!("Failed to emit tagged ODS counters: {err:#}"),
            Err(_) => warn!("Timed out emitting tagged ODS counters"),
        }
    }

    /// Run [`Self::send_tagged`] on a Tokio task and return at once, so the
    /// caller's work does not wait for the transport.
    ///
    /// # Panics
    ///
    /// Outside a Tokio runtime, when `points` is not empty.
    pub fn spawn_send_tagged(self, points: Vec<OdsPoint>, timeout: Duration) -> BackgroundSend {
        if points.is_empty() {
            return BackgroundSend::none();
        }
        BackgroundSend(Some(tokio::spawn(async move {
            self.send_tagged(points, timeout).await
        })))
    }
}

/// An ODS send that runs on a Tokio task. Await [`Self::finish`] to complete
/// it: dropping the value, or a `finish` future that has not completed,
/// cancels the send. The cancel takes effect at the next await point of the
/// send, so a send that another worker is running can still deliver its
/// points.
#[must_use = "await `finish` so that the datapoints are sent"]
pub struct BackgroundSend(Option<tokio::task::JoinHandle<()>>);

/// The payload of a panic in a background send.
pub type SendPanic = Box<dyn std::any::Any + Send + 'static>;

impl BackgroundSend {
    /// A send that has nothing to wait for.
    pub fn none() -> Self {
        Self(None)
    }

    /// Whether a send was started.
    pub fn is_started(&self) -> bool {
        self.0.is_some()
    }

    /// Wait for the send. Returns the payload of a panic in the send, so the
    /// caller decides whether to re-raise it.
    pub async fn finish(mut self) -> Result<(), SendPanic> {
        let Some(task) = self.0.as_mut() else {
            return Ok(());
        };
        let result = task.await;
        self.0 = None;
        match result {
            Ok(()) => Ok(()),
            Err(error) => match error.try_into_panic() {
                Ok(payload) => Err(payload),
                Err(error) => {
                    warn!("Background ODS send did not complete: {error:#}");
                    Ok(())
                }
            },
        }
    }
}

impl Drop for BackgroundSend {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

struct RouterSender;

#[async_trait]
impl OdsSender for RouterSender {
    async fn send(
        &self,
        fb: FacebookInit,
        source_entity: &str,
        category: &str,
        category_id: i32,
        points: Vec<OdsPoint>,
    ) -> anyhow::Result<()> {
        #[cfg(target_os = "linux")]
        {
            let client = make_OdsRouter_x2pclient!(fb, tiername = "ods_router.script")
                .context("Failed to create x2p ODS router client")?;
            let unix_time = i64::try_from(
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)?
                    .as_secs(),
            )?;
            client
                .setOdsRawValuesWithTags(&ods_request(
                    source_entity,
                    category,
                    category_id,
                    points,
                    unix_time,
                ))
                .await?;
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (fb, source_entity, category, category_id, points);
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn ods_request(
    source_entity: &str,
    category: &str,
    category_id: i32,
    points: Vec<OdsPoint>,
    unix_time: i64,
) -> SetOdsRawValuesWithTagsRequest {
    let data_points = points
        .into_iter()
        .map(|point| ODSAppValueWithTags {
            entity: source_entity.to_owned(),
            unixTime: unix_time,
            value: point.value,
            key: point.key,
            interval: ODS_INTERVAL_SECONDS,
            tags: vec![AggSettings {
                name: point.aggregate_entity,
                interval: ODS_INTERVAL_SECONDS,
                cross_datacenter: true,
                category_id,
                ..Default::default()
            }],
            category_id,
            hashes: vec![],
            ..Default::default()
        })
        .collect();
    SetOdsRawValuesWithTagsRequest {
        dataPoints: data_points,
        requireBufferAggregation: false,
        category: category.to_owned(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct FakeSender {
        sent: Arc<Mutex<Vec<(String, String, i32, Vec<OdsPoint>)>>>,
        fail: bool,
    }

    #[async_trait]
    impl OdsSender for FakeSender {
        async fn send(
            &self,
            _fb: FacebookInit,
            source_entity: &str,
            category: &str,
            category_id: i32,
            points: Vec<OdsPoint>,
        ) -> anyhow::Result<()> {
            self.sent.lock().unwrap().push((
                source_entity.to_owned(),
                category.to_owned(),
                category_id,
                points,
            ));
            if self.fail {
                anyhow::bail!("injected transport error");
            }
            Ok(())
        }
    }

    fn point(key: &str, aggregate_entity: &str, value: f64) -> OdsPoint {
        OdsPoint {
            key: key.to_owned(),
            value,
            aggregate_entity: aggregate_entity.to_owned(),
        }
    }

    #[fbinit::test]
    fn test_new_uses_bounded_host_source(fb: FacebookInit) {
        let sink = OdsSink::new(fb, "example_category", 41, "example").unwrap();
        let hostname = hostname::get().unwrap();
        assert_eq!(
            sink.source_entity,
            format!("example.host.{}", hostname.to_string_lossy())
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_request_tags_status_and_health_independently() {
        let request = ods_request(
            "example.host.rankhost.example",
            "example_category",
            41,
            vec![
                point("runs", "example.status.success", 1.0),
                point("queried", "example.health", 2.0),
                point("hits", "example.health", 0.0),
                point("errors", "example.health", 0.0),
            ],
            1_700_000_000,
        );
        assert_eq!(request.category, "example_category");
        assert!(!request.requireBufferAggregation);
        assert_eq!(request.dataPoints.len(), 4);
        for point in &request.dataPoints {
            assert_eq!(point.entity, "example.host.rankhost.example");
            assert_eq!(point.unixTime, 1_700_000_000);
            assert_eq!(point.interval, 60);
            assert_eq!(point.category_id, 41);
            assert_eq!(point.tags.len(), 1);
            assert_eq!(point.tags[0].interval, 60);
            assert_eq!(point.tags[0].category_id, 41);
            assert!(point.tags[0].cross_datacenter);
            if point.key == "runs" {
                assert_eq!(point.tags[0].name, "example.status.success");
            } else {
                assert_eq!(point.tags[0].name, "example.health");
            }
        }
    }

    #[fbinit::test]
    async fn test_fake_sender_sends_once_and_failure_is_nonfatal(fb: FacebookInit) {
        let fake = FakeSender {
            fail: true,
            ..Default::default()
        };
        let sent = Arc::clone(&fake.sent);
        let sink = OdsSink::with_sender(
            fb,
            "example_category",
            41,
            "example.host.rankhost.example".to_owned(),
            Box::new(fake),
        );
        sink.send_tagged(
            vec![point("runs", "example.status.success", 1.0)],
            Duration::from_secs(2),
        )
        .await;
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "example.host.rankhost.example");
        assert_eq!(sent[0].1, "example_category");
        assert_eq!(sent[0].2, 41);
        assert_eq!(sent[0].3.len(), 1);
    }

    struct PendingSender;

    #[async_trait]
    impl OdsSender for PendingSender {
        async fn send(
            &self,
            _fb: FacebookInit,
            _source_entity: &str,
            _category: &str,
            _category_id: i32,
            _points: Vec<OdsPoint>,
        ) -> anyhow::Result<()> {
            std::future::pending().await
        }
    }

    #[fbinit::test]
    async fn test_spawned_send_does_not_block_and_finish_waits_for_it(fb: FacebookInit) {
        let fake = FakeSender::default();
        let sent = Arc::clone(&fake.sent);
        let sink = sink_with(fb, Box::new(fake));

        // The test runtime has one thread, so the task cannot run before
        // this test awaits.
        let background =
            sink.spawn_send_tagged(vec![point("runs", "status", 1.0)], Duration::from_secs(60));
        assert!(sent.lock().unwrap().is_empty());

        background.finish().await.unwrap();
        assert_eq!(sent.lock().unwrap().len(), 1);
    }

    struct PanickingSender;

    #[async_trait]
    impl OdsSender for PanickingSender {
        async fn send(
            &self,
            _fb: FacebookInit,
            _source_entity: &str,
            _category: &str,
            _category_id: i32,
            _points: Vec<OdsPoint>,
        ) -> anyhow::Result<()> {
            panic!("transport panicked")
        }
    }

    /// A sender that never finishes and records when its send is dropped.
    struct DropRecordingSender(Arc<std::sync::atomic::AtomicBool>);

    struct SetOnDrop(Arc<std::sync::atomic::AtomicBool>);

    impl Drop for SetOnDrop {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl OdsSender for DropRecordingSender {
        async fn send(
            &self,
            _fb: FacebookInit,
            _source_entity: &str,
            _category: &str,
            _category_id: i32,
            _points: Vec<OdsPoint>,
        ) -> anyhow::Result<()> {
            let _guard = SetOnDrop(Arc::clone(&self.0));
            std::future::pending().await
        }
    }

    fn sink_with(fb: FacebookInit, sender: Box<dyn OdsSender>) -> OdsSink {
        OdsSink::with_sender(fb, "example_category", 41, "host.test".to_owned(), sender)
    }

    /// A send that never finishes, and the flag that its drop sets.
    fn pending_send(fb: FacebookInit) -> (BackgroundSend, Arc<std::sync::atomic::AtomicBool>) {
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let background = sink_with(fb, Box::new(DropRecordingSender(Arc::clone(&dropped))))
            .spawn_send_tagged(vec![point("runs", "status", 1.0)], Duration::from_secs(60));
        (background, dropped)
    }

    /// Whether `flag` is set within a few scheduler turns.
    async fn is_set_soon(flag: &std::sync::atomic::AtomicBool) -> bool {
        for _ in 0..10 {
            if flag.load(std::sync::atomic::Ordering::SeqCst) {
                return true;
            }
            tokio::task::yield_now().await;
        }
        flag.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[fbinit::test]
    async fn test_finish_returns_the_panic_of_the_send(fb: FacebookInit) {
        let panic = sink_with(fb, Box::new(PanickingSender))
            .spawn_send_tagged(vec![point("runs", "status", 1.0)], Duration::from_secs(60))
            .finish()
            .await
            .expect_err("finish returns the panic");
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"transport panicked"));
    }

    #[fbinit::test]
    async fn test_dropping_a_background_send_cancels_it(fb: FacebookInit) {
        let (background, dropped) = pending_send(fb);
        // Let the send start and wait in its transport.
        tokio::task::yield_now().await;

        drop(background);
        assert!(is_set_soon(&dropped).await);
    }

    #[fbinit::test]
    async fn test_a_send_without_points_starts_no_task(fb: FacebookInit) {
        assert!(
            !sink_with(fb, Box::<FakeSender>::default())
                .spawn_send_tagged(Vec::new(), Duration::from_secs(60))
                .is_started()
        );
    }

    #[fbinit::test]
    async fn test_dropping_an_unfinished_finish_cancels_the_send(fb: FacebookInit) {
        let (background, dropped) = pending_send(fb);

        tokio::time::timeout(Duration::from_millis(10), background.finish())
            .await
            .expect_err("the send never finishes");
        assert!(is_set_soon(&dropped).await);
    }

    #[fbinit::test]
    async fn test_spawned_send_keeps_the_timeout(fb: FacebookInit) {
        let background = sink_with(fb, Box::new(PendingSender))
            .spawn_send_tagged(vec![point("runs", "status", 1.0)], Duration::from_millis(1));
        tokio::time::timeout(Duration::from_secs(10), background.finish())
            .await
            .expect("the spawned send must stop at its own timeout")
            .unwrap();
    }

    #[fbinit::test]
    async fn test_finish_of_a_cancelled_send_returns_ok(_fb: FacebookInit) {
        let task = tokio::spawn(std::future::pending::<()>());
        task.abort();
        assert!(BackgroundSend(Some(task)).finish().await.is_ok());
    }

    #[fbinit::test]
    async fn test_timeout_returns_without_failing(fb: FacebookInit) {
        let sink = OdsSink::with_sender(
            fb,
            "example_category",
            41,
            "host.test".to_owned(),
            Box::new(PendingSender),
        );
        sink.send_tagged(vec![point("runs", "status", 1.0)], Duration::from_millis(1))
            .await;
    }
}
