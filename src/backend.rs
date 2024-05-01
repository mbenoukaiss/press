use std::error::Error;
use std::fs::{File, Metadata};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{BufReader, Read, Take};
use std::os::unix::fs::MetadataExt;
use chrono::{DateTime, Utc};
use varnish::vcl::backend::{Serve, Transfer};
use varnish::vcl::ctx::Ctx;

pub struct FileBackend {
    pub path: String,
}

impl Serve<FileTransfer> for FileBackend<> {
    fn get_type(&self) -> &str {
        "fileserver"
    }

    fn get_headers(&self, ctx: &mut Ctx) -> Result<Option<FileTransfer>, Box<dyn Error>> {
        // we know that bereq and bereq_url, so we can just unwrap the options
        let bereq = ctx.http_bereq.as_ref().unwrap();
        let bereq_url = bereq.url().unwrap();

        // combine root and url into something that's hopefully safe
        let path = assemble_file_path(&self.path, bereq_url);
        ctx.log(varnish::vcl::ctx::LogTag::Debug, &format!("fileserver: file on disk: {:?}", path));

        // reset the bereq lifetime, otherwise we couldn't use ctx in the line above
        // yes, it feels weird at first, but it's for our own good
        let bereq = ctx.http_bereq.as_ref().unwrap();

        // let's start building our response
        let beresp = ctx.http_beresp.as_mut().unwrap();

        // open the file and get some metadata
        let f = std::fs::File::open(&path).map_err(|e| e.to_string())?;
        let metadata: Metadata = f.metadata().map_err(|e| e.to_string())?;
        let cl = metadata.len();
        let modified: DateTime<Utc> = DateTime::from(metadata.modified().unwrap());
        let etag = generate_etag(&metadata);

        // can we avoid sending a body?
        let mut is_304 = false;
        if let Some(inm) = bereq.header("if-none-match") {
            if inm == etag || (inm.starts_with("W/") && inm[2..] == etag) {
                is_304 = true;
            }
        } else if let Some(ims) = bereq.header("if-modified-since") {
            if let Ok(t) = DateTime::parse_from_rfc2822(ims) {
                if t > modified {
                    is_304 = true;
                }
            }
        }

        beresp.set_proto("HTTP/1.1")?;
        let mut transfer = None;
        if bereq.method() != Some("HEAD") && bereq.method() != Some("GET") {
            // we are fairly strict in what method we accept
            beresp.set_status(405);
            return Ok(None);
        } else if is_304 {
            // 304 will save us some bandwidth
            beresp.set_status(304);
        } else {
            // "normal" request, if it's a HEAD to save a bunch of work, but if
            // it's a GET we need to add the VFP to the pipeline
            // and add a BackendResp to the priv1 field
            beresp.set_status(200);
            if bereq.method() == Some("GET") {
                transfer = Some(FileTransfer {
                    // prevent reading more than expected
                    reader: std::io::BufReader::new(f).take(cl)
                });
            }
        }

        // set all the headers we can, including the content-type if we can
        beresp.set_header("content-length", &format!("{}", cl))?;
        beresp.set_header("etag", &etag)?;
        beresp.set_header("last-modified", &modified.format("%a, %d %b %Y %H:%M:%S GMT").to_string())?;
        beresp.set_header("content-type", "image/webp")?;

        Ok(transfer)
    }
}

pub struct FileTransfer {
    reader: Take<BufReader<File>>,
}

impl Transfer for FileTransfer {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Box<dyn Error>> {
        self.reader.read(buf).map_err(|e| e.into())
    }

    fn len(&self) -> Option<usize> {
        Some(self.reader.limit() as usize)
    }
}

// given root_path and url, assemble the two so that the final path is still
// inside root_path
// There's no access to the file system, and therefore no link resolution
// it can be an issue for multitenancy, beware!
fn assemble_file_path(root_path: &str, url: &str) -> std::path::PathBuf {
    assert_ne!(root_path, "");

    let url_path = std::path::PathBuf::from(url);
    let mut components = Vec::new();

    for c in url_path.components() {
        use std::path::Component::*;
        match c {
            Prefix(_) => unreachable!(),
            RootDir => {}
            CurDir => (),
            ParentDir => { components.pop(); }
            Normal(s) => {
                // we can unwrap as url_path was created from an &str
                components.push(s.to_str().unwrap());
            }
        };
    }

    let mut complete_path = String::from(root_path);
    for c in components {
        complete_path.push('/');
        complete_path.push_str(c);
    }
    std::path::PathBuf::from(complete_path)
}

fn generate_etag(metadata: &std::fs::Metadata) -> String {
    #[derive(Hash)]
    struct ShortMd {
        inode: u64,
        size: u64,
        modified: std::time::SystemTime,
    }

    let smd = ShortMd {
        inode: metadata.ino(),
        size: metadata.size(),
        modified: metadata.modified().unwrap(),
    };
    let mut h = DefaultHasher::new();
    smd.hash(&mut h);
    format!("\"{}\"", h.finish())
}

#[cfg(test)]
mod tests {
    use super::assemble_file_path;

    fn tc(root_path: &str, url: &str, expected: &str) {
        assert_eq!(assemble_file_path(root_path, url), std::path::PathBuf::from(expected));
    }

    #[test]
    fn simple() { tc("/foo/bar", "/baz/qux", "/foo/bar/baz/qux"); }

    #[test]
    fn simple_slash() { tc("/foo/bar/", "/baz/qux", "/foo/bar/baz/qux"); }

    #[test]
    fn parent() { tc("/foo/bar", "/bar/../qux", "/foo/bar/qux"); }

    #[test]
    fn too_many_parents() { tc("/foo/bar", "/bar/../../qux", "/foo/bar/qux"); }

    #[test]
    fn current() { tc("/foo/bar", "/bar/././qux", "/foo/bar/bar/qux"); }
}