use std;
use {DataAddress, DataType, Digest, Error, Repo, DIGEST_SIZE};
use VerifyResults;
use std::io;
use std::cell::RefCell;
use std::collections::HashSet;
use std::io::Write;
use {ArcCompression, ArcDecrypter};
use slog::{FnValue, Logger};
use hex::ToHex;

/// Translates index stream into data stream
///
/// This type implements `io::Write` and interprets what's written to it as a
/// stream of digests.
///
/// For every digest written to it, it will access the corresponding chunk and
/// write it into `writer` that it wraps.
struct IndexTranslator<'a, 'b> {
    writer: Option<&'b mut Write>,
    digest_buf: Digest,
    data_type: DataType,
    read_context: &'a ReadContext<'a>,
    log: Logger,
}

impl<'a, 'b> IndexTranslator<'a, 'b> {
    pub(crate) fn new(
        writer: Option<&'b mut Write>,
        data_type: DataType,
        read_context: &'a ReadContext<'a>,
        log: Logger,
    ) -> Self {
        IndexTranslator {
            data_type: data_type,
            digest_buf: Digest(Vec::with_capacity(DIGEST_SIZE)),
            read_context: read_context,
            writer: writer,
            log: log,
        }
    }
}

impl<'a, 'b> Write for IndexTranslator<'a, 'b> {
    // TODO: This is copying too much. Could be not copying anything, unless
    // bytes < DIGEST_SIZE
    fn write(&mut self, mut bytes: &[u8]) -> io::Result<usize> {
        assert!(!bytes.is_empty());

        let total_len = bytes.len();
        loop {
            let has_already = self.digest_buf.0.len();
            if (has_already + bytes.len()) < DIGEST_SIZE {
                self.digest_buf.0.extend_from_slice(bytes);

                trace!(self.log, "left with a buffer";
                       "digest" => FnValue(|_| self.digest_buf.0.to_hex()),
                       );
                return Ok(total_len);
            }

            let needs = DIGEST_SIZE - has_already;
            self.digest_buf.0.extend_from_slice(&bytes[..needs]);
            debug_assert_eq!(self.digest_buf.0.len(), DIGEST_SIZE);

            bytes = &bytes[needs..];
            let &mut IndexTranslator {
                ref mut digest_buf,
                data_type,
                ref mut writer,
                read_context,
                ..
            } = self;
            let res = if let Some(ref mut writer) = *writer {
                read_context.read_recursively(ReadRequest::new(
                    data_type,
                    &DataAddress {
                        digest: digest_buf,
                        index_level: 0,
                    },
                    Some(writer),
                    self.log.clone(),
                ))
            } else {
                read_context.accessor.touch(digest_buf)
            };
            digest_buf.0.clear();
            res?;
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a, 'b> Drop for IndexTranslator<'a, 'b> {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            debug_assert_eq!(self.digest_buf.0.len(), 0);
        }
    }
}

/// Information specific to a given read operation
/// of a data in the Repo
pub(crate) struct ReadRequest<'a> {
    data_address: &'a DataAddress<'a>,
    data_type: DataType,
    writer: Option<&'a mut Write>,
    log: Logger,
}

impl<'a> ReadRequest<'a> {
    pub(crate) fn new(
        data_type: DataType,
        data_address: &'a DataAddress,
        writer: Option<&'a mut Write>,
        log: Logger,
    ) -> Self {
        ReadRequest {
            data_type: data_type,
            data_address: data_address,
            writer: writer,
            log: log,
        }
    }
}

/// Read Context
///
/// Information about the `Repo` that is open for reaading
pub(crate) struct ReadContext<'a> {
    /// Writer to write the data to; `None` will discard the data
    accessor: &'a ChunkAccessor,
}

impl<'a> ReadContext<'a> {
    pub(crate) fn new(accessor: &'a ChunkAccessor) -> Self {
        ReadContext { accessor: accessor }
    }

    fn on_index(&self, mut req: ReadRequest) -> io::Result<()> {
        trace!(req.log, "Traversing index";
               "digest" => FnValue(|_| req.data_address.digest.0.to_hex()),
               );

        let mut translator = IndexTranslator::new(
            req.writer.take(),
            req.data_type,
            self,
            req.log.clone(),
        );

        let da = DataAddress {
            digest: req.data_address.digest,
            index_level: req.data_address.index_level - 1,
        };
        let req = ReadRequest::new(
            DataType::Index,
            &da,
            Some(&mut translator),
            req.log,
        );
        self.read_recursively(req)
    }

    fn on_data(&self, mut req: ReadRequest) -> io::Result<()> {
        trace!(req.log, "Traversing data";
               "digest" => FnValue(|_| req.data_address.digest.0.to_hex()),
               );
        if let Some(writer) = req.writer.take() {
            self.accessor.read_chunk_into(
                req.data_address.digest,
                req.data_type,
                writer,
            )
        } else {
            self.accessor.touch(req.data_address.digest)
        }
    }

    pub(crate) fn read_recursively(&self, req: ReadRequest) -> io::Result<()> {
        trace!(req.log, "Reading recursively";
               "digest" => FnValue(|_| req.data_address.digest.0.to_hex()),
               );

        if req.data_address.index_level == 0 {
            self.on_data(req)
        } else {
            self.on_index(req)
        }
    }
}


/// Abstraction over accessing chunks stored in the repository
pub(crate) trait ChunkAccessor {
    fn repo(&self) -> &Repo;

    /// Read a chunk identified by `digest` into `writer`
    fn read_chunk_into(
        &self,
        digest: &Digest,
        data_type: DataType,
        writer: &mut Write,
    ) -> io::Result<()>;


    fn touch(&self, _digest: &Digest) -> io::Result<()> {
        Ok(())
    }
}

/// `ChunkAccessor` that just reads the chunks as requested, without doing
/// anything
pub(crate) struct DefaultChunkAccessor<'a> {
    repo: &'a Repo,
    decrypter: Option<ArcDecrypter>,
    compression: ArcCompression,
}

impl<'a> DefaultChunkAccessor<'a> {
    pub(crate) fn new(
        repo: &'a Repo,
        decrypter: Option<ArcDecrypter>,
        compression: ArcCompression,
    ) -> Self {
        DefaultChunkAccessor {
            repo: repo,
            decrypter: decrypter,
            compression: compression,
        }
    }
}

impl<'a> ChunkAccessor for DefaultChunkAccessor<'a> {
    fn repo(&self) -> &Repo {
        self.repo
    }

    fn read_chunk_into(
        &self,
        digest: &Digest,
        data_type: DataType,
        writer: &mut Write,
    ) -> io::Result<()> {
        let path = self.repo.chunk_rel_path_by_digest(digest);
        let data = self.repo.aio.read(path).wait()?;

        let data = if data_type.should_encrypt() {
            self.decrypter
                .as_ref()
                .expect("Decrypter expected")
                .decrypt(data, &digest.0)?
        } else {
            data
        };

        let data = if data_type.should_compress() {
            self.compression.decompress(data)?
        } else {
            data
        };

        let vec_result = self.repo.hasher.calculate_digest(&data);

        if vec_result != digest.0 {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} corrupted, data read: {}",
                    digest.0.to_hex(),
                    vec_result.to_hex()
                ),
            ))
        } else {
            for part in data.as_parts() {
                writer.write_all(&*part)?;
            }
            Ok(())
        }
    }
}

/// `ChunkAccessor` that records which chunks
/// were accessed
///
/// This is useful for chunk garbage-collection
pub(crate) struct RecordingChunkAccessor<'a> {
    raw: DefaultChunkAccessor<'a>,
    accessed: RefCell<&'a mut HashSet<Vec<u8>>>,
}

impl<'a> RecordingChunkAccessor<'a> {
    pub(crate) fn new(
        repo: &'a Repo,
        accessed: &'a mut HashSet<Vec<u8>>,
        decrypter: Option<ArcDecrypter>,
        compression: ArcCompression,
    ) -> Self {
        RecordingChunkAccessor {
            raw: DefaultChunkAccessor::new(repo, decrypter, compression),
            accessed: RefCell::new(accessed),
        }
    }
}

impl<'a> ChunkAccessor for RecordingChunkAccessor<'a> {
    fn repo(&self) -> &Repo {
        self.raw.repo()
    }

    fn touch(&self, digest: &Digest) -> io::Result<()> {
        self.accessed.borrow_mut().insert(digest.0.clone());
        Ok(())
    }

    fn read_chunk_into(
        &self,
        digest: &Digest,
        data_type: DataType,
        writer: &mut Write,
    ) -> io::Result<()> {
        self.touch(digest)?;
        self.raw.read_chunk_into(digest, data_type, writer)
    }
}

/// `ChunkAccessor` that verifies the chunks
/// that are accessed
///
/// This is used to verify a name / index
pub(crate) struct VerifyingChunkAccessor<'a> {
    raw: DefaultChunkAccessor<'a>,
    accessed: RefCell<HashSet<Vec<u8>>>,
    errors: RefCell<Vec<(Vec<u8>, Error)>>,
}

impl<'a> VerifyingChunkAccessor<'a> {
    pub(crate) fn new(
        repo: &'a Repo,
        decrypter: Option<ArcDecrypter>,
        compression: ArcCompression,
    ) -> Self {
        VerifyingChunkAccessor {
            raw: DefaultChunkAccessor::new(repo, decrypter, compression),
            accessed: RefCell::new(HashSet::new()),
            errors: RefCell::new(Vec::new()),
        }
    }

    pub(crate) fn get_results(self) -> VerifyResults {
        VerifyResults {
            scanned: self.accessed.borrow().len(),
            errors: self.errors.into_inner(),
        }
    }
}

impl<'a> ChunkAccessor for VerifyingChunkAccessor<'a> {
    fn repo(&self) -> &Repo {
        self.raw.repo()
    }

    fn read_chunk_into(
        &self,
        digest: &Digest,
        data_type: DataType,
        writer: &mut Write,
    ) -> io::Result<()> {
        {
            let mut accessed = self.accessed.borrow_mut();
            if accessed.contains(&digest.0) {
                return Ok(());
            }
            accessed.insert(digest.0.clone());
        }
        let res = self.raw.read_chunk_into(digest, data_type, writer);

        if res.is_err() {
            self.errors
                .borrow_mut()
                .push((digest.0.clone(), res.err().unwrap()));
        }
        Ok(())
    }
}