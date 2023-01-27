use std::{
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use async_compression::tokio::write::ZstdEncoder;
use futures_core::future::BoxFuture;
use matrix_sdk::async_trait;
use opentelemetry::{
    sdk::{
        export::trace::ExportResult,
        trace::{BatchMessage, TraceRuntime, Tracer},
        util::tokio_interval_stream,
    },
    trace::TraceError,
};
use opentelemetry_contrib::trace::exporter::jaeger_json::JaegerJsonRuntime;
use tokio::{
    fs::{DirEntry, File},
    io::{AsyncReadExt, AsyncWriteExt},
    runtime::Handle,
};
use tokio_tar::Builder;

/// This is only used on ios for now, so let's allow dead code.
#[allow(dead_code)]
pub fn create_rageshake_tracer(path: PathBuf, client_name: &str, runtime: Handle) -> Tracer {
    use opentelemetry_contrib::trace::exporter::jaeger_json::JaegerJsonExporter;

    let tracer_runtime = RageshakeTracingRuntime {
        file_prefix: format!("{client_name}-trace"),
        runtime: runtime.to_owned(),
        directory: path.to_owned(),
    };

    let _guard = runtime.enter();

    JaegerJsonExporter::new(
        path,
        tracer_runtime.file_prefix.to_owned(),
        client_name.to_owned(),
        tracer_runtime,
    )
    .install_batch()
}

#[derive(Clone, Debug)]
struct RageshakeTracingRuntime {
    file_prefix: String,
    runtime: Handle,
    directory: PathBuf,
}

impl RageshakeTracingRuntime {
    const MAX_PLAINTEXT_FILES: usize = 200;
    const COMPRESSED_FILES_PER_PLAINTEXT: usize = 200;

    async fn is_a_trace_file(&self, entry: &DirEntry) -> std::io::Result<bool> {
        let file_type = entry.file_type().await?;
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();

        Ok(file_name.starts_with(&self.file_prefix)
            && file_name.ends_with("json")
            && file_type.is_file())
    }

    async fn collect_and_compress(&self) -> std::io::Result<()> {
        let mut dir = tokio::fs::read_dir(&self.directory).await?;
        let mut files: Vec<_> = vec![];

        while let Some(item) = dir.next_entry().await? {
            if self.is_a_trace_file(&item).await? {
                files.push(item);
            }
        }

        if files.len() >= Self::MAX_PLAINTEXT_FILES {
            self.compress_and_cleanup(files).await?;
        }

        Ok(())
    }

    async fn archive(path: &Path, files: &[DirEntry]) -> std::io::Result<()> {
        let archive = File::create(path).await?;
        let mut archiver = Builder::new(archive);

        for item in files {
            archiver.append_path_with_name(item.path(), item.file_name()).await?;
        }

        archiver.finish().await?;

        Ok(())
    }

    async fn compress(archive_path: &Path) -> std::io::Result<()> {
        let mut buffer = [0u8; 2048];

        let mut file_path = archive_path.to_owned();
        file_path.set_extension("tar.zstd");

        let mut archive = File::open(&archive_path).await?;
        let output_file = File::create(file_path).await?;
        let mut encoder = ZstdEncoder::new(output_file);

        loop {
            let read = archive.read(&mut buffer).await?;

            if read == 0 {
                break;
            } else {
                encoder.write(&buffer[..read]).await?;
            }
        }

        encoder.shutdown().await?;

        Ok(())
    }

    /// Collect all trace files we found into an archive and compress it, remove
    /// the source JSON files afterwards.
    async fn compress_and_cleanup(&self, mut files: Vec<DirEntry>) -> std::io::Result<()> {
        files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

        let chunks = files.chunks_exact(Self::COMPRESSED_FILES_PER_PLAINTEXT);

        let now = std::time::SystemTime::now();
        let duration = now
            .duration_since(UNIX_EPOCH)
            .expect("We can always calculate the unix timestamp")
            .as_secs();

        // We don't want to create a huge tar bomb, so let's limit the amount of files
        // per archive.
        for (chunk_number, files) in chunks.enumerate() {
            let mut path = self.directory.to_owned();

            let file_name = if chunk_number == 0 {
                format!("weechat-matrix-traces-{}", duration)
            } else {
                format!("weechat-matrix-traces-{}-{}", duration, chunk_number)
            };

            path.push(file_name);

            let mut archive_path = path.to_owned();
            archive_path.set_extension("tar");

            Self::archive(&archive_path, files).await?;
            Self::compress(&archive_path).await?;

            tokio::fs::remove_file(archive_path).await?;

            for file in files {
                tokio::fs::remove_file(file.path()).await?;
            }
        }

        Ok(())
    }
}

impl opentelemetry::runtime::Runtime for RageshakeTracingRuntime {
    type Interval = tokio_stream::wrappers::IntervalStream;
    type Delay = ::std::pin::Pin<Box<tokio::time::Sleep>>;

    fn interval(&self, duration: std::time::Duration) -> Self::Interval {
        let _guard = self.runtime.enter();
        tokio_interval_stream(duration)
    }

    fn spawn(&self, future: BoxFuture<'static, ()>) {
        let _ = self.runtime.spawn(future);
    }

    fn delay(&self, duration: std::time::Duration) -> Self::Delay {
        let _guard = self.runtime.enter();
        Box::pin(tokio::time::sleep(duration))
    }
}

impl TraceRuntime for RageshakeTracingRuntime {
    type Receiver = tokio_stream::wrappers::ReceiverStream<BatchMessage>;
    type Sender = tokio::sync::mpsc::Sender<BatchMessage>;

    fn batch_message_channel(&self, capacity: usize) -> (Self::Sender, Self::Receiver) {
        let (sender, receiver) = tokio::sync::mpsc::channel(capacity);
        (sender, tokio_stream::wrappers::ReceiverStream::new(receiver))
    }
}

#[async_trait]
impl JaegerJsonRuntime for RageshakeTracingRuntime {
    async fn create_dir(&self, path: &Path) -> ExportResult {
        let _guard = self.runtime.enter();
        let path = path.to_owned();

        if tokio::fs::metadata(&path).await.is_err() {
            tokio::fs::create_dir_all(path).await.map_err(|e| TraceError::Other(Box::new(e)))
        } else {
            Ok(())
        }
    }

    async fn write_to_file(&self, path: &Path, content: &[u8]) -> ExportResult {
        let _guard = self.runtime.enter();
        use tokio::io::AsyncWriteExt;
        let path = path.to_owned();
        let content = content.to_owned();

        let mut file = File::create(path).await.map_err(|e| TraceError::Other(Box::new(e)))?;
        file.write_all(&content).await.map_err(|e| TraceError::Other(Box::new(e)))?;
        file.sync_data().await.map_err(|e| TraceError::Other(Box::new(e)))?;

        // Should we put this in the background?
        self.collect_and_compress().await.map_err(|e| TraceError::Other(Box::new(e)))?;

        Ok(())
    }
}
