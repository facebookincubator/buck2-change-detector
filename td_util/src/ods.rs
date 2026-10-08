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
