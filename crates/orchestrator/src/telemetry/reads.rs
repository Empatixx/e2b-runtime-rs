use std::sync::LazyLock;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram, Meter};

use crate::storage::frame_table::CompressionType;

const METER_NAME: &str = "github.com/e2b-dev/infra/packages/shared/pkg/storage";
const SUB_MILLISECOND_BUCKETS: [f64; 17] = [
    0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0,
];
const TRANSITIONED_TEXT: &str = "peer upload completed, reload header from storage";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Unknown,
    Mmap,
    Fs,
    Peer,
    Nfs,
    Gcs,
    Aws,
    Azure,
}

impl Source {
    pub const ALL: [Source; 8] = [
        Source::Unknown,
        Source::Mmap,
        Source::Fs,
        Source::Peer,
        Source::Nfs,
        Source::Gcs,
        Source::Aws,
        Source::Azure,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Source::Unknown => "unknown",
            Source::Mmap => "mmap",
            Source::Fs => "fs",
            Source::Peer => "peer",
            Source::Nfs => "nfs",
            Source::Gcs => "gcs",
            Source::Aws => "aws",
            Source::Azure => "azure",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekableObjectType {
    Unknown,
    Memfile,
    Rootfs,
}

impl SeekableObjectType {
    pub fn label(self) -> &'static str {
        match self {
            SeekableObjectType::Memfile => "memfile",
            SeekableObjectType::Rootfs => "rootfs",
            SeekableObjectType::Unknown => "unknown",
        }
    }

    pub fn of_path(path: &str) -> Self {
        match path.rsplit('/').next().unwrap_or(path) {
            "memfile" => SeekableObjectType::Memfile,
            "rootfs.ext4" => SeekableObjectType::Rootfs,
            _ => SeekableObjectType::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    NotFound,
    ErrCanceled,
    ErrIo,
    ErrTimeout,
    Transitioned,
    Contended,
}

impl Outcome {
    pub fn label(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::NotFound => "not_found",
            Outcome::ErrCanceled => "err_canceled",
            Outcome::ErrIo => "err_io",
            Outcome::ErrTimeout => "err_timeout",
            Outcome::Transitioned => "transitioned",
            Outcome::Contended => "contended",
        }
    }

    pub fn of(result: Result<(), &str>) -> Self {
        match result {
            Ok(()) => Outcome::Ok,
            Err(error) => Self::of_error(error),
        }
    }

    pub fn of_error(error: &str) -> Self {
        match error {
            error if error.contains(TRANSITIONED_TEXT) => Outcome::Transitioned,
            error if error.contains("object does not exist") || error.contains("No such file or directory") => {
                Outcome::NotFound
            }
            error if error.contains("context canceled") => Outcome::ErrCanceled,
            error if error.contains("timed out") || error.contains("deadline exceeded") => Outcome::ErrTimeout,
            _ => Outcome::ErrIo,
        }
    }
}

pub fn blob_type(path: &str) -> &'static str {
    let name = path.rsplit('/').next().unwrap_or(path);
    let name = [".zstd", ".lz4"]
        .iter()
        .find_map(|suffix| name.strip_suffix(suffix))
        .unwrap_or(name);
    match name {
        name if name.ends_with(".header") => "header",
        "snapfile" => "snapfile",
        "metadata.json" => "metadata",
        _ => "other",
    }
}

#[derive(Debug, Clone)]
pub struct Timer {
    duration: Histogram<f64>,
    bytes: Counter<u64>,
    count: Counter<u64>,
}

impl Timer {
    fn new(meter: &Meter, name: &'static str, duration: &'static str, bytes: &'static str) -> Self {
        Self {
            duration: meter
                .f64_histogram(name)
                .with_description(duration)
                .with_unit("ms")
                .with_boundaries(SUB_MILLISECOND_BUCKETS.to_vec())
                .build(),
            bytes: meter
                .u64_counter(format!("{name}.size"))
                .with_description(bytes)
                .with_unit("By")
                .build(),
            count: meter
                .u64_counter(name)
                .with_description(format!("Total {name} events recorded"))
                .build(),
        }
    }

    fn record(&self, took: Duration, total: i64, attributes: &[KeyValue]) {
        self.duration.record(milliseconds(took), attributes);
        self.bytes.add(total.max(0) as u64, attributes);
        self.count.add(1, attributes);
    }
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

#[derive(Debug, Clone)]
pub struct ReadMetrics {
    open: Timer,
    read: Timer,
    decompress: Timer,
    fetch: Timer,
    efficiency: Histogram<f64>,
    size_duration: Histogram<f64>,
    size_count: Counter<u64>,
    blob: Timer,
    blob_decompress: Timer,
}

impl ReadMetrics {
    pub fn new(meter: &Meter) -> Self {
        Self {
            open: Timer::new(
                meter,
                "orchestrator.read.open",
                "OpenRangeReader (open / TTFB) wall",
                "Bytes (always 0 \u{2014} open transfers no payload)",
            ),
            read: Timer::new(
                meter,
                "orchestrator.read.read",
                "Raw source-read wall (decompression excluded)",
                "Compressed/stored bytes read from the source",
            ),
            decompress: Timer::new(
                meter,
                "orchestrator.read.decompress",
                "Decompression CPU wall (decoder read time minus source transfer)",
                "Uncompressed bytes produced",
            ),
            fetch: Timer::new(
                meter,
                "orchestrator.read.fetch",
                "Total fetch wall \u{2014} should \u{2248} open + read + decompress; any excess is overhead (see read.pipeline.efficiency)",
                "Bytes delivered to the app",
            ),
            efficiency: meter
                .f64_histogram("orchestrator.read.pipeline.efficiency")
                .with_description(
                    "fetch / (open + read + decompress) \u{2014} 1.0 = fetch wall fully explained by work, >1 = overhead",
                )
                .with_unit("1")
                .build(),
            size_duration: meter
                .f64_histogram("orchestrator.read.size")
                .with_description("Size() metadata-lookup wall")
                .with_unit("ms")
                .with_boundaries(SUB_MILLISECOND_BUCKETS.to_vec())
                .build(),
            size_count: meter
                .u64_counter("orchestrator.read.size")
                .with_description("Total orchestrator.read.size events recorded")
                .build(),
            blob: Timer::new(
                meter,
                "orchestrator.read.blob",
                "Whole-object blob transfer (WriteTo) wall",
                "Bytes transferred",
            ),
            blob_decompress: Timer::new(
                meter,
                "orchestrator.read.blob.decompress",
                "Blob deserialize/decompress wall (e.g. header LZ4 decode + parse)",
                "Uncompressed bytes produced",
            ),
        }
    }

    pub fn record_open(
        &self,
        took: Duration,
        object: SeekableObjectType,
        source: Source,
        codec: CompressionType,
        outcome: Outcome,
    ) {
        self.open
            .record(took, 0, &read_attributes(object, source, codec, outcome));
    }

    pub fn record_read(&self, took: Duration, bytes: i64, attributes: &[KeyValue]) {
        self.read.record(took, bytes, attributes);
    }

    pub fn record_decompress(&self, took: Duration, bytes: i64, attributes: &[KeyValue]) {
        self.decompress.record(took, bytes, attributes);
    }

    pub fn record_fetch(&self, took: Duration, bytes: i64, attributes: &[KeyValue]) {
        self.fetch.record(took, bytes, attributes);
    }

    pub fn record_pipeline_efficiency(&self, ratio: f64, attributes: &[KeyValue]) {
        self.efficiency.record(ratio, attributes);
    }

    pub fn record_size(&self, took: Duration, object: SeekableObjectType, source: Source, outcome: Outcome) {
        let attributes = read_attributes(object, source, CompressionType::None, outcome);
        self.size_duration.record(milliseconds(took), &attributes);
        self.size_count.add(1, &attributes);
    }

    pub fn record_blob(&self, took: Duration, bytes: i64, path: &str, source: Source, outcome: Outcome) {
        self.blob.record(
            took,
            bytes,
            &[
                KeyValue::new("file_type", blob_type(path)),
                KeyValue::new("source", source.label()),
                KeyValue::new("codec", CompressionType::None.to_string()),
                KeyValue::new("outcome", outcome.label()),
            ],
        );
    }

    pub fn record_blob_decompress(
        &self,
        took: Duration,
        bytes: i64,
        path: &str,
        codec: CompressionType,
        outcome: Outcome,
    ) {
        self.blob_decompress.record(
            took,
            bytes,
            &[
                KeyValue::new("file_type", blob_type(path)),
                KeyValue::new("codec", codec.to_string()),
                KeyValue::new("outcome", outcome.label()),
            ],
        );
    }
}

pub fn read_attributes(
    object: SeekableObjectType,
    source: Source,
    codec: CompressionType,
    outcome: Outcome,
) -> [KeyValue; 4] {
    [
        KeyValue::new("file_type", object.label()),
        KeyValue::new("source", source.label()),
        KeyValue::new("codec", codec.to_string()),
        KeyValue::new("outcome", outcome.label()),
    ]
}

static READS: LazyLock<ReadMetrics> = LazyLock::new(|| ReadMetrics::new(&global::meter(METER_NAME)));

pub fn reads() -> &'static ReadMetrics {
    &READS
}

#[cfg(test)]
mod tests {
    use opentelemetry::metrics::MeterProvider;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    use super::*;

    fn provider() -> (SdkMeterProvider, InMemoryMetricExporter) {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        (provider, exporter)
    }

    #[derive(Debug, PartialEq)]
    struct Point {
        name: String,
        unit: String,
        attributes: Vec<(String, String)>,
        value: f64,
    }

    fn points(exporter: &InMemoryMetricExporter) -> Vec<Point> {
        let attributes = |keys: &mut dyn Iterator<Item = &KeyValue>| {
            let mut pairs: Vec<_> = keys
                .map(|kv| (kv.key.to_string(), kv.value.as_str().into_owned()))
                .collect();
            pairs.sort();
            pairs
        };
        exporter
            .get_finished_metrics()
            .unwrap()
            .iter()
            .flat_map(|resource| resource.scope_metrics())
            .flat_map(|scope| scope.metrics())
            .flat_map(|metric| {
                let (name, unit) = (metric.name().to_owned(), metric.unit().to_owned());
                match metric.data() {
                    AggregatedMetrics::U64(MetricData::Sum(sum)) => sum
                        .data_points()
                        .map(|point| Point {
                            name: name.clone(),
                            unit: unit.clone(),
                            attributes: attributes(&mut point.attributes()),
                            value: point.value() as f64,
                        })
                        .collect::<Vec<_>>(),
                    AggregatedMetrics::F64(MetricData::Histogram(histogram)) => histogram
                        .data_points()
                        .map(|point| Point {
                            name: name.clone(),
                            unit: unit.clone(),
                            attributes: attributes(&mut point.attributes()),
                            value: point.sum(),
                        })
                        .collect::<Vec<_>>(),
                    _ => Vec::new(),
                }
            })
            .collect()
    }

    fn point(points: &[Point], name: &str, unit: &str) -> Vec<(Vec<(String, String)>, f64)> {
        points
            .iter()
            .filter(|point| point.name == name && point.unit == unit)
            .map(|point| (point.attributes.clone(), point.value))
            .collect()
    }

    fn pairs(values: &[(&str, &str)]) -> Vec<(String, String)> {
        values.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    #[test]
    fn every_source_has_a_label() {
        assert!(Source::ALL.iter().all(|source| !source.label().is_empty()));
        assert_eq!(Source::Peer.label(), "peer");
        assert_eq!(Source::Mmap.label(), "mmap");
    }

    #[test]
    fn seekable_object_types_have_the_upstream_labels() {
        assert_eq!(SeekableObjectType::Memfile.label(), "memfile");
        assert_eq!(SeekableObjectType::Rootfs.label(), "rootfs");
        assert_eq!(SeekableObjectType::Unknown.label(), "unknown");
        assert_eq!(
            SeekableObjectType::of_path("build/rootfs.ext4"),
            SeekableObjectType::Rootfs
        );
        assert_eq!(
            SeekableObjectType::of_path("build/snapfile"),
            SeekableObjectType::Unknown
        );
    }

    #[test]
    fn errors_map_to_the_outcome_buckets() {
        assert_eq!(Outcome::of(Ok(())), Outcome::Ok);
        assert_eq!(Outcome::of(Err("object does not exist")), Outcome::NotFound);
        assert_eq!(Outcome::of(Err("fetch x: timed out after 60s")), Outcome::ErrTimeout);
        assert_eq!(Outcome::of(Err("context canceled")), Outcome::ErrCanceled);
        assert_eq!(
            Outcome::of(Err("peer upload completed, reload header from storage")),
            Outcome::Transitioned
        );
        assert_eq!(Outcome::of(Err("boom")), Outcome::ErrIo);
        assert_eq!(Outcome::Contended.label(), "contended");
    }

    #[test]
    fn blob_paths_are_classified_into_a_fixed_set() {
        assert_eq!(blob_type("b/memfile.header"), "header");
        assert_eq!(blob_type("b/rootfs.ext4.header"), "header");
        assert_eq!(blob_type("b/snapfile"), "snapfile");
        assert_eq!(blob_type("b/metadata.json"), "metadata");
        assert_eq!(blob_type("b/anything-123"), "other");
        assert_eq!(blob_type("snapfile"), "snapfile");
    }

    #[test]
    fn an_open_is_a_duration_a_zero_byte_counter_and_a_count_with_the_upstream_names() {
        let (provider, exporter) = provider();
        let metrics = ReadMetrics::new(&provider.meter("test"));

        metrics.record_open(
            Duration::from_millis(3),
            SeekableObjectType::Memfile,
            Source::Peer,
            CompressionType::None,
            Outcome::Ok,
        );
        provider.force_flush().unwrap();
        let points = points(&exporter);

        let attributes = pairs(&[
            ("codec", "none"),
            ("file_type", "memfile"),
            ("outcome", "ok"),
            ("source", "peer"),
        ]);
        assert_eq!(
            point(&points, "orchestrator.read.open", "ms"),
            vec![(attributes.clone(), 3.0)]
        );
        assert_eq!(
            point(&points, "orchestrator.read.open.size", "By"),
            vec![(attributes.clone(), 0.0)]
        );
        assert_eq!(point(&points, "orchestrator.read.open", ""), vec![(attributes, 1.0)]);
    }

    #[test]
    fn a_size_lookup_records_a_duration_and_a_count_under_one_name() {
        let (provider, exporter) = provider();
        let metrics = ReadMetrics::new(&provider.meter("test"));

        metrics.record_size(
            Duration::from_millis(2),
            SeekableObjectType::Rootfs,
            Source::Gcs,
            Outcome::NotFound,
        );
        provider.force_flush().unwrap();
        let points = points(&exporter);

        let attributes = pairs(&[
            ("codec", "none"),
            ("file_type", "rootfs"),
            ("outcome", "not_found"),
            ("source", "gcs"),
        ]);
        assert_eq!(
            point(&points, "orchestrator.read.size", "ms"),
            vec![(attributes.clone(), 2.0)]
        );
        assert_eq!(point(&points, "orchestrator.read.size", ""), vec![(attributes, 1.0)]);
    }

    #[test]
    fn a_blob_read_carries_the_blob_type_and_the_bytes_transferred() {
        let (provider, exporter) = provider();
        let metrics = ReadMetrics::new(&provider.meter("test"));

        metrics.record_blob(Duration::from_millis(5), 11, "b/snapfile", Source::Peer, Outcome::Ok);
        provider.force_flush().unwrap();
        let points = points(&exporter);

        let attributes = pairs(&[
            ("codec", "none"),
            ("file_type", "snapfile"),
            ("outcome", "ok"),
            ("source", "peer"),
        ]);
        assert_eq!(
            point(&points, "orchestrator.read.blob.size", "By"),
            vec![(attributes, 11.0)]
        );
    }

    #[test]
    fn a_blob_decompress_has_a_codec_and_no_source() {
        let (provider, exporter) = provider();
        let metrics = ReadMetrics::new(&provider.meter("test"));

        metrics.record_blob_decompress(
            Duration::from_millis(1),
            100,
            "b/memfile.header",
            CompressionType::Lz4,
            Outcome::Ok,
        );
        provider.force_flush().unwrap();
        let points = points(&exporter);

        let attributes = pairs(&[("codec", "lz4"), ("file_type", "header"), ("outcome", "ok")]);
        assert_eq!(
            point(&points, "orchestrator.read.blob.decompress.size", "By"),
            vec![(attributes, 100.0)]
        );
    }

    #[test]
    fn the_fetch_read_and_efficiency_instruments_use_their_upstream_names_and_units() {
        let (provider, exporter) = provider();
        let metrics = ReadMetrics::new(&provider.meter("test"));
        let attributes = read_attributes(
            SeekableObjectType::Memfile,
            Source::Gcs,
            CompressionType::None,
            Outcome::Ok,
        );

        metrics.record_fetch(Duration::from_millis(4), 4096, &attributes);
        metrics.record_read(Duration::from_millis(3), 4096, &attributes);
        metrics.record_decompress(Duration::from_millis(1), 4096, &attributes);
        metrics.record_pipeline_efficiency(1.25, &attributes);
        provider.force_flush().unwrap();
        let points = points(&exporter);

        let names: Vec<_> = points
            .iter()
            .map(|point| (point.name.as_str(), point.unit.as_str()))
            .collect();
        for expected in [
            ("orchestrator.read.fetch", "ms"),
            ("orchestrator.read.fetch.size", "By"),
            ("orchestrator.read.read", "ms"),
            ("orchestrator.read.read.size", "By"),
            ("orchestrator.read.decompress", "ms"),
            ("orchestrator.read.pipeline.efficiency", "1"),
        ] {
            assert!(names.contains(&expected), "{expected:?} missing from {names:?}");
        }
    }
}
