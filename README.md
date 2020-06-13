<h1 align="center">form-data</h1>

<div align="center">
  <p><strong>AsyncRead/AsyncWrite/Stream for `multipart/form-data` <sup>rfc7578</sup></strong></p>
</div>

<div align="center">
  <!-- Safety docs -->
  <a href="/">
    <img src="https://img.shields.io/badge/-safety!-success?style=flat-square" alt="Safety!" /></a>
  <!-- Docs.rs docs -->
  <a href="https://docs.rs/form-data">
    <img src="https://img.shields.io/badge/docs-latest-blue.svg?style=flat-square"
      alt="Docs.rs docs" /></a>
  <!-- Crates version -->
  <a href="https://crates.io/crates/form-data">
    <img src="https://img.shields.io/crates/v/form-data.svg?style=flat-square"
    alt="Crates.io version" /></a>
  <!-- Downloads -->
  <a href="https://crates.io/crates/form-data">
    <img src="https://img.shields.io/crates/d/form-data.svg?style=flat-square"
      alt="Download" /></a>
  <!-- Twitter -->
  <a href="https://twitter.com/_fundon">
    <img src="https://img.shields.io/badge/twitter-@__fundon-blue.svg?style=flat-square" alt="Twitter: @_fundon" /></a>
</div>

## Features

- **Stream**: `Form`, `Field`

- **AsyncRead**: `Field`, so easy `read`/`copy` field data to anywhere.

- **Fast**: Hyper supports bigger buffer by defaults, over 8KB, up to 512KB possible.

    If we want to receive large buffer, and save them to writer or file. See [hyper example](examples/hyper.rs)

    -. Set `max_buf_size` to FormData, `form_data.set_max_buf_size(512 * 1024)?;`.

    -. Use `copy_to`, copy bigger buffer to a writer(`AsyncRead`), `field.copy_to(&mut writer)`.

    -. Use `copy_to_file`, copy bigger buffer to a file(`File`), `field.copy_to_file(file)`.

- Preparse headers of part

## Example

Request payload, the example from [jaydenseric/graphql-multipart-request-spec](https://github.com/jaydenseric/graphql-multipart-request-spec#request-payload-2).

```txt
--------------------------627436eaefdbc285
Content-Disposition: form-data; name="operations"

[{ "query": "mutation ($file: Upload!) { singleUpload(file: $file) { id } }", "variables": { "file": null } }, { "query": "mutation($files: [Upload!]!) { multipleUpload(files: $files) { id } }", "variables": { "files": [null, null] } }]
--------------------------627436eaefdbc285
Content-Disposition: form-data; name="map"

{ "0": ["0.variables.file"], "1": ["1.variables.files.0"], "2": ["1.variables.files.1"] }
--------------------------627436eaefdbc285
Content-Disposition: form-data; name="0"; filename="a.txt"
Content-Type: text/plain

Alpha file content.

--------------------------627436eaefdbc285
Content-Disposition: form-data; name="1"; filename="b.txt"
Content-Type: text/plain

Bravo file content.

--------------------------627436eaefdbc285
Content-Disposition: form-data; name="2"; filename="c.txt"
Content-Type: text/plain

Charlie file content.

--------------------------627436eaefdbc285--
```

[tests/hyper-body.rs](hyper-body)

```rust
use std::fs::{self, File};

use anyhow::{anyhow, Result};
use bytes::BytesMut;
use hyper::Body;
use tempfile::tempdir;

use futures_util::io::{self, AsyncReadExt, AsyncWriteExt};
use futures_util::stream::TryStreamExt;

use form_data::*;

mod limited;
use limited::Limited;

#[test]
fn hyper_body() -> Result<()> {
    pretty_env_logger::try_init()?;

    smol::block_on(async {
        let payload = smol::reader(File::open("tests/fixtures/graphql.txt")?);
        let stream = Limited::random_with(payload, 256);

        let body = Body::wrap_stream(stream);
        let mut form = FormData::new("------------------------627436eaefdbc285", body);

        while let Some(mut field) = form.try_next().await? {
            assert!(!field.consumed());
            assert_eq!(field.length, 0);

            match field.index {
                Some(0) => {
                    assert_eq!(field.name, "operations");
                    assert_eq!(field.filename, None);
                    assert_eq!(field.content_type, None);

                    // read chunks
                    let mut buffer = BytesMut::new();
                    while let Some(buf) = field.try_next().await? {
                        buffer.extend_from_slice(&buf);
                    }

                    assert_eq!(buffer, "[{ \"query\": \"mutation ($file: Upload!) { singleUpload(file: $file) { id } }\", \"variables\": { \"file\": null } }, { \"query\": \"mutation($files: [Upload!]!) { multipleUpload(files: $files) { id } }\", \"variables\": { \"files\": [null, null] } }]");
                    assert_eq!(field.length, buffer.len() as u64);

                    assert!(field.consumed());

                    log::info!("{:#?}", field);
                }
                Some(1) => {
                    assert_eq!(field.name, "map");
                    assert_eq!(field.filename, None);
                    assert_eq!(field.content_type, None);

                    // read bytes
                    let buffer = field.bytes().await?;

                    assert_eq!(buffer, "{ \"0\": [\"0.variables.file\"], \"1\": [\"1.variables.files.0\"], \"2\": [\"1.variables.files.1\"] }");
                    assert_eq!(field.length, buffer.len() as u64);

                    assert!(field.consumed());

                    log::info!("{:#?}", field);
                }
                Some(2) => {
                    assert_eq!(field.name, "0");
                    assert_eq!(field.filename, Some("a.txt".into()));
                    assert_eq!(field.content_type, Some(mime::TEXT_PLAIN));

                    let dir = tempdir()?;

                    let filename = field.filename.as_ref().unwrap();
                    let filepath = dir.path().join(filename);

                    let mut writer = smol::writer(File::create(&filepath)?);

                    // copy data to file
                    let bytes = io::copy(field, &mut writer).await?;
                    writer.close().await?;

                    // async ?
                    let metadata = fs::metadata(&filepath)?;
                    assert_eq!(metadata.len(), bytes);

                    let mut reader = smol::reader(File::open(&filepath)?);
                    let mut contents = Vec::new();
                    reader.read_to_end(&mut contents).await?;
                    assert_eq!(contents, "Alpha file content.\r\n".as_bytes());

                    dir.close()?;
                }
                Some(3) => {
                    assert_eq!(field.name, "1");
                    assert_eq!(field.filename, Some("b.txt".into()));
                    assert_eq!(field.content_type, Some(mime::TEXT_PLAIN));

                    // read data to buffer
                    let mut buffer = Vec::with_capacity(4);
                    let bytes = field.read_to_end(&mut buffer).await?;

                    assert_eq!(buffer, "Bravo file content.\r\n".as_bytes());
                    assert_eq!(field.length, bytes as u64);
                    assert_eq!(field.length, buffer.len() as u64);
                }
                Some(4) => {
                    assert_eq!(field.name, "2");
                    assert_eq!(field.filename, Some("c.txt".into()));
                    assert_eq!(field.content_type, Some(mime::TEXT_PLAIN));

                    // read data to string
                    let mut string = String::new();
                    let bytes = field.read_to_string(&mut string).await?;

                    assert_eq!(string, "Charlie file content.\r\n");
                    assert_eq!(field.length, bytes as u64);
                    assert_eq!(field.length, string.len() as u64);
                }
                _ => {}
            }
        }

        let state = form.state();
        let state = state.try_lock().map_err(|e| anyhow!(e.to_string()))?;

        assert_eq!(state.eof(), true);
        assert_eq!(state.total(), 5);
        assert_eq!(state.len(), 1029);

        Ok(())
    })
}
```

## License

<sup>
Licensed under either of <a href="LICENSE-APACHE">Apache License, Version
2.0</a> or <a href="LICENSE-MIT">MIT license</a> at your option.
</sup>

<br>

<sub>
Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
</sub>
