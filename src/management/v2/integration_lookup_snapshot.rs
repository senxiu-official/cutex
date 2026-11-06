//! Immutable journal byte boundaries for lookup construction outside both locks.
use super::super::model::{EventEnvelope, EventStreamMetadata};
use anyhow::Context;
use std::{
    collections::VecDeque,
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Take},
    path::PathBuf,
};

pub(crate) type LookupCheckpoint = (String, String, u64, u64);
pub(crate) struct SnapshotFile {
    pub path: PathBuf,
    pub file: File,
    pub length: u64,
}
pub(crate) struct ReadSnapshot {
    pub metadata: EventStreamMetadata,
    pub files: Vec<SnapshotFile>,
}
pub(crate) struct Reader {
    files: VecDeque<SnapshotFile>,
    current: Option<(BufReader<Take<File>>, PathBuf, u64)>,
    stream: String,
    next: u64,
    target: u64,
    pub(crate) bytes: u64,
}

fn decoded(bytes: &[u8], stream: &str) -> anyhow::Result<EventEnvelope> {
    #[cfg(test)]
    assert!(
        !crate::management::v2::integration_events::lookup_lock_held(),
        "lookup parsing under integration lock"
    );
    let event: EventEnvelope = serde_json::from_slice(bytes)?;
    event.validate()?;
    anyhow::ensure!(event.stream_id == stream, "snapshot mixes journal streams");
    Ok(event)
}

fn ending_at(file: &mut File, end: u64, stream: &str) -> anyhow::Result<EventEnvelope> {
    anyhow::ensure!(end > 0, "empty snapshot record");
    let amount = end.min((super::super::event_blobs::MAX_EVENT_BYTES + 1) as u64);
    file.seek(SeekFrom::Start(end - amount))?;
    let mut bytes = vec![0; amount as usize];
    file.read_exact(&mut bytes)?;
    anyhow::ensure!(
        bytes.last() == Some(&b'\n'),
        "snapshot has a partial journal tail"
    );
    bytes.pop();
    let start = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |n| n + 1);
    decoded(&bytes[start..], stream)
}

impl ReadSnapshot {
    pub(crate) fn event_at(
        self,
        stream: &str,
        cursor: &str,
        sequence: u64,
    ) -> anyhow::Result<Option<EventEnvelope>> {
        if self.metadata.stream_id != stream
            || self
                .metadata
                .earliest
                .as_ref()
                .is_none_or(|e| sequence < e.sequence)
            || self
                .metadata
                .latest
                .as_ref()
                .is_none_or(|e| sequence > e.sequence)
        {
            return Ok(None);
        }
        for mut entry in self.files {
            if entry.length == 0 {
                continue;
            }
            let last = ending_at(&mut entry.file, entry.length, stream)?;
            if sequence > last.sequence {
                continue;
            }
            entry.file.seek(SeekFrom::Start(0))?;
            let mut first = String::new();
            BufReader::new(&mut entry.file).read_line(&mut first)?;
            let first = decoded(first.as_bytes(), stream)?;
            if sequence < first.sequence {
                continue;
            }
            for event in [first, last] {
                if event.sequence == sequence {
                    anyhow::ensure!(event.cursor == cursor, "snapshot cursor sequence mismatch");
                    return Ok(Some(event));
                }
            }
            // Search bytes, then decode only the matching record. This must
            // not initialize the repository's whole hot replay index.
            entry.file.seek(SeekFrom::Start(0))?;
            let mut reader = entry.file.take(entry.length);
            let search = super::ByteSearch::new(
                format!("\"cursor\":{}", serde_json::to_string(cursor)?).into_bytes(),
            );
            let mut buffer = Vec::new();
            loop {
                let kept = buffer.len();
                buffer.resize(kept + 256 * 1024, 0);
                let read = reader.read(&mut buffer[kept..])?;
                buffer.truncate(kept + read);
                let complete = buffer
                    .iter()
                    .rposition(|b| *b == b'\n')
                    .map_or(0, |n| n + 1);
                if complete == 0 {
                    if read == 0 {
                        break;
                    }
                    continue;
                }
                let mut from = 0;
                while let Some(hit) = search.find(&buffer[..complete], from) {
                    let start = buffer[..hit]
                        .iter()
                        .rposition(|b| *b == b'\n')
                        .map_or(0, |n| n + 1);
                    let end = hit
                        + buffer[hit..complete]
                            .iter()
                            .position(|b| *b == b'\n')
                            .unwrap();
                    let event = decoded(&buffer[start..end], stream)?;
                    if event.cursor == cursor {
                        anyhow::ensure!(
                            event.sequence == sequence,
                            "snapshot cursor sequence mismatch"
                        );
                        return Ok(Some(event));
                    }
                    from = end + 1;
                }
                if read == 0 {
                    break;
                }
                buffer.drain(..complete);
            }
        }
        anyhow::bail!("retained integration cursor is missing from its immutable snapshot")
    }
    pub(crate) fn reader(self, checkpoint: Option<&LookupCheckpoint>) -> anyhow::Result<Reader> {
        let stream = self.metadata.stream_id;
        let target = self.metadata.latest.as_ref().map_or(0, |e| e.sequence);
        let earliest = self.metadata.earliest.as_ref().map_or(1, |e| e.sequence);
        // Old cache checkpoints lack offsets/sequence and require one rebuild.
        let checkpoint = checkpoint.filter(|cp| cp.0 == stream && cp.2 >= earliest && cp.3 > 0);
        if checkpoint.is_some_and(|cp| cp.2 >= target) {
            return Ok(Reader {
                files: VecDeque::new(),
                current: None,
                stream,
                next: target + 1,
                target,
                bytes: 0,
            });
        }
        let next = checkpoint.map_or(earliest, |cp| cp.2 + 1);
        let mut files = VecDeque::new();
        let mut current = None;
        for mut entry in self.files {
            if entry.length == 0 {
                continue;
            }
            let mut offset = 0;
            if let Some(cp) = checkpoint {
                let last = ending_at(&mut entry.file, entry.length, &stream)?;
                if last.sequence <= cp.2 {
                    if last.sequence == cp.2 {
                        anyhow::ensure!(last.cursor == cp.1, "snapshot checkpoint cursor mismatch");
                    }
                    continue;
                }
                entry.file.seek(SeekFrom::Start(0))?;
                let mut first = String::new();
                BufReader::new(&mut entry.file).read_line(&mut first)?;
                let first = decoded(first.as_bytes(), &stream)?;
                if first.sequence <= cp.2 {
                    anyhow::ensure!(
                        cp.3 <= entry.length,
                        "snapshot checkpoint offset exceeds file"
                    );
                    let at = ending_at(&mut entry.file, cp.3, &stream)?;
                    anyhow::ensure!(
                        at.sequence == cp.2 && at.cursor == cp.1,
                        "snapshot checkpoint offset mismatch"
                    );
                    offset = cp.3;
                }
            }
            if current.is_none() {
                entry.file.seek(SeekFrom::Start(offset))?;
                current = Some((
                    BufReader::new(entry.file.take(entry.length - offset)),
                    entry.path,
                    offset,
                ));
            } else {
                files.push_back(entry);
            }
        }
        Ok(Reader {
            files,
            current,
            stream,
            next,
            target,
            bytes: 0,
        })
    }
}

impl Reader {
    /// A published checkpoint describes a fully validated prefix. An interrupted
    /// batch never moves it; callers cannot infer absence from a partial build.
    pub(crate) fn batch(
        &mut self,
        limit: usize,
    ) -> anyhow::Result<Option<(Vec<EventEnvelope>, LookupCheckpoint)>> {
        let mut events = Vec::new();
        let mut end = None;
        while events.len() < limit && self.next <= self.target {
            if self.current.is_none() {
                let Some(mut entry) = self.files.pop_front() else {
                    break;
                };
                entry.file.seek(SeekFrom::Start(0))?;
                self.current = Some((BufReader::new(entry.file.take(entry.length)), entry.path, 0));
            }
            let (reader, path, offset) = self.current.as_mut().unwrap();
            let mut line = String::new();
            let count = reader
                .read_line(&mut line)
                .with_context(|| format!("read lookup snapshot {}", path.display()))?;
            if count == 0 {
                self.current = None;
                continue;
            }
            anyhow::ensure!(
                line.ends_with('\n'),
                "snapshot has a partial journal record"
            );
            let event = decoded(line.as_bytes(), &self.stream)?;
            anyhow::ensure!(
                event.sequence == self.next,
                "snapshot sequence gap: expected {}, found {}",
                self.next,
                event.sequence
            );
            *offset += count as u64;
            self.next += 1;
            self.bytes += count as u64;
            end = Some((
                self.stream.clone(),
                event.cursor.clone(),
                event.sequence,
                *offset,
            ));
            events.push(event);
        }
        if let Some(end) = end {
            Ok(Some((events, end)))
        } else {
            anyhow::ensure!(
                self.next > self.target,
                "snapshot ended before its committed target"
            );
            Ok(None)
        }
    }
}
