//! The portal's site: the reference web app, `web/src/`, embedded in the
//! binary, assembled once at startup by the rules of `web/build.py`, branded by `[portal] site` and `overlay`,
//! and served from memory. A request never touches the filesystem, and the binary writes no value into a page:
//! each page reads its figures from `/api/v1` in the browser.
//!
//! The rules are build.py's, to the byte, which the e2e suite checks (`test_portal_site.py`), so a change to
//! one is a change to the other. The site's API is always the binary's own origin, so a site's `api` is
//! ignored.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use axum::{
    body::Bytes,
    http::{HeaderName, HeaderValue, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use rust_embed::RustEmbed;
use serde::Deserialize;

/// The app's files, as `web/src/` holds them.
#[derive(RustEmbed)]
#[folder = "web/src/"]
struct App;

/// The site without `[portal] site`: PuntoHunter's own.
const DEFAULT_SITE: &str = include_str!("../../web/site.json");

/// The directory of the files pages include, which is not served.
const PARTIALS: &str = "partials";
/// How many times includes are expanded, so an include inside a partial is followed to that depth.
const PASSES: usize = 5;
/// A page that carries this is left out of search, so it gets no canonical link.
const NOINDEX: &str = r#"<meta name="robots" content="noindex">"#;
/// The page a path that names no file gets.
const NOT_FOUND: &str = "404.html";
/// The config.js build.py writes for a site whose API is on its own origin, as the binary's always is.
const CONFIG_JS: &str = "\
// Written by build.py from site.json; edit that, not this.
// Where the PuntoHunter pool serves its API: the origin in front of /api/v1. Empty means this site's own
// origin, where the host passes /api/v1 through to the pool.
export const API = \"\";
";

/// What a page's meta tags cannot carry, as `web/hosting/` adds it on a static host.
const HEADERS: [(HeaderName, &str); 4] = [
    (header::CONTENT_SECURITY_POLICY, "frame-ancestors 'none'"),
    (header::REFERRER_POLICY, "no-referrer"),
    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    (
        HeaderName::from_static("permissions-policy"),
        "camera=(), microphone=(), geolocation=()",
    ),
];

#[derive(Debug, thiserror::Error)]
#[error("portal site {path}: {reason}")]
pub struct SiteError {
    path: String,
    reason: String,
}

fn error(path: impl std::fmt::Display, reason: impl std::fmt::Display) -> SiteError {
    SiteError {
        path: path.to_string(),
        reason: reason.to_string(),
    }
}

/// A site's `site.json`. Its `api` and any other key are ignored.
#[derive(Deserialize)]
struct SiteJson {
    name: String,
    description: String,
    title: Option<String>,
    origin: Option<String>,
}

/// One source file, and where it came from, for an error to name.
struct Source {
    data: Vec<u8>,
    from: String,
}

/// A file as it is served.
struct File {
    body: Bytes,
    content_type: &'static str,
}

/// The assembled site, by path without its leading `/`.
pub struct Site {
    name: String,
    files: BTreeMap<String, File>,
}

impl Site {
    /// The app with the site `site.json` names, or PuntoHunter's own, and `overlay` laid over it.
    pub fn build(site: Option<&Path>, overlay: Option<&Path>) -> Result<Self, SiteError> {
        let site = match site {
            Some(path) => {
                let text = std::fs::read_to_string(path).map_err(|e| error(path.display(), e))?;
                parse_site(&text).map_err(|reason| error(path.display(), reason))?
            }
            None => parse_site(DEFAULT_SITE).map_err(|reason| error("web/site.json", reason))?,
        };
        let base = App::iter()
            .map(|path| {
                let data = App::get(&path).expect("an embedded file").data.into_owned();
                let from = format!("web/src/{path}");
                (path.into_owned(), Source { data, from })
            })
            .collect();
        let overlay = match overlay {
            Some(dir) => read_overlay(dir)?,
            None => BTreeMap::new(),
        };
        assemble(base, overlay, &site)
    }

    /// The site's name, as its pages give it.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// How many files the site serves.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// The file `uri` names, `/` the overview, or the 404 page.
    pub fn response(&self, uri: &Uri) -> Response {
        let path = match uri.path() {
            "/" => "index.html",
            path => path.trim_start_matches('/'),
        };
        match self.files.get(path) {
            Some(file) => served(StatusCode::OK, path, file),
            None => match self.files.get(NOT_FOUND) {
                Some(file) => served(StatusCode::NOT_FOUND, NOT_FOUND, file),
                None => StatusCode::NOT_FOUND.into_response(),
            },
        }
    }
}

fn served(status: StatusCode, path: &str, file: &File) -> Response {
    // The vendored files carry their version in their names, so they never change under a URL.
    let cache = if path.starts_with("vendor/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let mut response = (status, file.body.clone()).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(file.content_type),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    for (name, value) in HEADERS {
        headers.insert(name, HeaderValue::from_static(value));
    }
    response
}

/// `site.json`'s values, checked as build.py checks them.
fn parse_site(text: &str) -> Result<SiteJson, String> {
    let mut site: SiteJson = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if let Some(origin) = &mut site.origin {
        let trimmed = origin.trim_end_matches('/').len();
        origin.truncate(trimmed);
        if !origin.is_empty() && !is_origin(origin) {
            return Err(format!(
                "origin must be an origin such as https://pool.example.com, not {origin:?}"
            ));
        }
    }
    Ok(site)
}

/// Whether `text` is http or https, a host name or a bracketed IPv6 literal, and an optional port, and nothing
/// else, as build.py's ORIGIN pattern admits.
fn is_origin(text: &str) -> bool {
    let Some(rest) = text
        .strip_prefix("https://")
        .or_else(|| text.strip_prefix("http://"))
    else {
        return false;
    };
    let (host, port) = match rest.strip_prefix('[') {
        Some(v6) => match v6.split_once(']') {
            Some((literal, port)) => {
                if literal.is_empty()
                    || !literal.bytes().all(|b| b.is_ascii_hexdigit() || b == b':')
                {
                    return false;
                }
                ("[]", port)
            }
            None => return false,
        },
        None => match rest.find(':') {
            Some(colon) => (&rest[..colon], &rest[colon..]),
            None => (rest, ""),
        },
    };
    let host_ok = host == "[]"
        || (!host.is_empty()
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-'));
    let port_ok = port.is_empty()
        || port
            .strip_prefix(':')
            .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()));
    host_ok && port_ok
}

/// Every file under `dir`, by its path from `dir` with `/` between parts. Symlinked directories are not
/// followed, as build.py's `rglob` does not follow them.
fn read_overlay(dir: &Path) -> Result<BTreeMap<String, Source>, SiteError> {
    fn walk(
        root: &Path,
        dir: &Path,
        files: &mut BTreeMap<String, Source>,
    ) -> Result<(), SiteError> {
        let entries = std::fs::read_dir(dir).map_err(|e| error(dir.display(), e))?;
        for entry in entries {
            let entry = entry.map_err(|e| error(dir.display(), e))?;
            let path = entry.path();
            let kind = entry.file_type().map_err(|e| error(path.display(), e))?;
            if kind.is_dir() {
                walk(root, &path, files)?;
            } else if path.is_file() {
                let rel = relative(root, &path)?;
                let data = std::fs::read(&path).map_err(|e| error(path.display(), e))?;
                let from = path.display().to_string();
                files.insert(rel, Source { data, from });
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    walk(dir, dir, &mut files)?;
    Ok(files)
}

fn relative(root: &Path, path: &Path) -> Result<String, SiteError> {
    let rel: PathBuf = path.strip_prefix(root).expect("under the root").into();
    let parts: Option<Vec<&str>> = rel.iter().map(|part| part.to_str()).collect();
    parts
        .map(|parts| parts.join("/"))
        .ok_or_else(|| error(path.display(), "the name is not UTF-8"))
}

/// The site from `base`, `web/src/`'s files, with `overlay`'s replacing those of the same path or added.
fn assemble(
    base: BTreeMap<String, Source>,
    overlay: BTreeMap<String, Source>,
    site: &SiteJson,
) -> Result<Site, SiteError> {
    let mut sources = base;
    sources.extend(overlay);
    let values = [
        ("name", site.name.as_str()),
        (
            "title",
            site.title
                .as_deref()
                .filter(|title| !title.is_empty())
                .unwrap_or(site.name.as_str()),
        ),
        ("description", site.description.as_str()),
        ("connect_src", "'self'"),
    ];
    let origin = site.origin.as_deref().unwrap_or_default();
    let mut files = BTreeMap::new();
    for (path, source) in &sources {
        if path.split('/').next() == Some(PARTIALS) {
            continue;
        }
        let name = path.rsplit('/').next().unwrap_or(path);
        let body = if name.ends_with(".html") && name != ".html" {
            let text = page(source, &sources, &values)?;
            canonical(text, path, origin).into_bytes()
        } else {
            source.data.clone()
        };
        files.insert(path.clone(), file(path, body));
    }
    files.insert("config.js".to_owned(), file("config.js", CONFIG_JS.into()));
    Ok(Site {
        name: site.name.clone(),
        files,
    })
}

fn file(path: &str, body: Vec<u8>) -> File {
    File {
        body: body.into(),
        content_type: content_type(path),
    }
}

fn content_type(path: &str) -> &'static str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.').map(|(_, extension)| extension) {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("webp") => "image/webp",
        Some("woff2") => "font/woff2",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// A source file as text, its line ends made `\n`, as Python's `read_text` reads it.
fn text(source: &Source) -> Result<String, SiteError> {
    let text = std::str::from_utf8(&source.data).map_err(|e| error(&source.from, e))?;
    Ok(text.replace("\r\n", "\n").replace('\r', "\n"))
}

/// The page `source`, with its partials included and its `{{keys}}` filled in.
fn page(
    source: &Source,
    sources: &BTreeMap<String, Source>,
    values: &[(&str, &str)],
) -> Result<String, SiteError> {
    let mut text = text(source)?;
    for _ in 0..PASSES {
        let (expanded, count) = include(&text, sources)?;
        text = expanded;
        if count == 0 {
            break;
        }
    }
    fill(&text, values).map_err(|key| {
        error(
            &source.from,
            format!("nothing in site.json for {{{{{key}}}}}"),
        )
    })
}

/// `text` with each `<!--#include NAME -->` replaced by `partials/NAME`, and how many it replaced. A line break
/// right after an include goes with it when the partial is empty, so an unused hook leaves no blank line.
fn include(text: &str, sources: &BTreeMap<String, Source>) -> Result<(String, usize), SiteError> {
    const OPEN: &str = "<!--#include";
    let mut out = String::with_capacity(text.len());
    let mut count = 0;
    let mut rest = text;
    while let Some(at) = rest.find(OPEN) {
        out.push_str(&rest[..at]);
        let candidate = &rest[at..];
        match directive(&candidate[OPEN.len()..]) {
            Some((name, len)) => {
                let path = format!("{PARTIALS}/{name}");
                let partial = sources
                    .get(&path)
                    .ok_or_else(|| error(format!("web/src/{path}"), "no such partial"))?;
                let partial = self::text(partial)?;
                let after = &candidate[OPEN.len() + len..];
                let newline = after.starts_with('\n');
                if !partial.is_empty() {
                    out.push_str(&partial);
                    if newline {
                        out.push('\n');
                    }
                }
                rest = if newline { &after[1..] } else { after };
                count += 1;
            }
            None => {
                out.push_str(OPEN);
                rest = &candidate[OPEN.len()..];
            }
        }
    }
    out.push_str(rest);
    Ok((out, count))
}

/// After `<!--#include`: whitespace, the partial's name, optional whitespace, and `-->`, as build.py's pattern
/// `\s+([\w.-]+)\s*-->` matches them; the name and the length of what matched.
fn directive(text: &str) -> Option<(&str, usize)> {
    let start = text.len() - text.trim_start().len();
    if start == 0 {
        return None;
    }
    let name_len = text[start..]
        .char_indices()
        .find(|&(_, c)| !(c.is_alphanumeric() || matches!(c, '_' | '.' | '-')))
        .map_or(text.len() - start, |(i, _)| i);
    // The name may end in dashes the closing `-->` needs, so it gives them back, longest name first.
    let ends = text[start..start + name_len]
        .char_indices()
        .map(|(i, c)| i + c.len_utf8())
        .rev();
    for end in ends {
        let after = &text[start + end..];
        let close = after.len() - after.trim_start().len();
        if after[close..].starts_with("-->") {
            return Some((&text[start..start + end], start + end + close + 3));
        }
    }
    None
}

/// `text` with each `{{key}}` filled from `values`, HTML-escaped; the first key with no value otherwise.
fn fill(text: &str, values: &[(&str, &str)]) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("{{") {
        out.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        let key_len = after
            .char_indices()
            .find(|&(_, c)| !(c.is_alphanumeric() || c == '_'))
            .map_or(after.len(), |(i, _)| i);
        if key_len > 0 && after[key_len..].starts_with("}}") {
            let key = &after[..key_len];
            let value = values
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| *value)
                .ok_or_else(|| key.to_owned())?;
            out.push_str(&escape(value));
            rest = &after[key_len + 2..];
        } else {
            // Not a key here; the next `{` may start one.
            out.push('{');
            rest = &rest[at + 1..];
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// `text` escaped as build.py escapes a value: `&`, `<`, `>`, and `"`.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `text` with its canonical link, when the site names its origin and search engines may index the page. The
/// binary serves each page at its own path, so the link names it with its `.html`.
fn canonical(text: String, path: &str, origin: &str) -> String {
    if origin.is_empty() || path == NOT_FOUND || text.contains(NOINDEX) {
        return text;
    }
    let mut address = format!("/{path}");
    if address.ends_with("/index.html") {
        address.truncate(address.len() - "index.html".len());
    }
    let link = format!(
        "<link rel=\"canonical\" href=\"{}\">\n",
        escape(&format!("{origin}{address}"))
    );
    text.replacen("</head>", &format!("{link}</head>"), 1)
}

#[cfg(test)]
mod tests {
    use bitcoin::hashes::{Hash, sha256};

    use super::*;

    fn source(text: &str) -> Source {
        Source {
            data: text.as_bytes().to_vec(),
            from: "test".to_owned(),
        }
    }

    fn sources(files: &[(&str, &str)]) -> BTreeMap<String, Source> {
        files
            .iter()
            .map(|(path, text)| ((*path).to_owned(), source(text)))
            .collect()
    }

    fn site_json(origin: Option<&str>) -> SiteJson {
        SiteJson {
            name: "Pool & \"Co\"".to_owned(),
            description: "<b>mining</b>".to_owned(),
            title: None,
            origin: origin.map(str::to_owned),
        }
    }

    fn built(files: &[(&str, &str)], overlay: &[(&str, &str)], origin: Option<&str>) -> Site {
        assemble(sources(files), sources(overlay), &site_json(origin)).unwrap()
    }

    fn body(site: &Site, path: &str) -> String {
        String::from_utf8(site.files[path].body.to_vec()).unwrap()
    }

    #[test]
    fn an_include_takes_its_partial_and_an_empty_one_its_line_break() {
        let site = built(
            &[
                (
                    "index.html",
                    "<!--#include head.html -->\n<!--#include  empty.html-->\nend\n",
                ),
                ("partials/head.html", "<head>"),
                ("partials/empty.html", ""),
            ],
            &[],
            None,
        );
        assert_eq!(body(&site, "index.html"), "<head>\nend\n");
        assert!(!site.files.contains_key("partials/head.html"), "not served");
    }

    #[test]
    fn a_name_gives_back_the_dashes_its_closing_needs() {
        let site = built(
            &[
                ("a.html", "[<!--#include x-y.html-->]"),
                ("partials/x-y.html", "X"),
            ],
            &[],
            None,
        );
        assert_eq!(body(&site, "a.html"), "[X]");
    }

    #[test]
    fn includes_nest_five_levels_and_no_deeper() {
        let mut files = vec![("a.html", "<!--#include p1 -->".to_owned())];
        for level in 1..=6 {
            files.push((
                Box::leak(format!("partials/p{level}").into_boxed_str()),
                format!("{level}<!--#include p{} -->", level + 1),
            ));
        }
        files.push(("partials/p7", "end".to_owned()));
        let files: Vec<(&str, &str)> = files.iter().map(|(p, t)| (*p, t.as_str())).collect();
        let site = built(&files, &[], None);
        assert_eq!(body(&site, "a.html"), "12345<!--#include p6 -->");
    }

    #[test]
    fn a_missing_partial_and_a_key_with_no_value_are_refused() {
        let err = assemble(
            sources(&[("a.html", "<!--#include gone.html -->")]),
            BTreeMap::new(),
            &site_json(None),
        )
        .err()
        .unwrap();
        assert_eq!(
            err.to_string(),
            "portal site web/src/partials/gone.html: no such partial"
        );
        let err = assemble(
            sources(&[("a.html", "{{colour}}")]),
            BTreeMap::new(),
            &site_json(None),
        )
        .err()
        .unwrap();
        assert_eq!(
            err.to_string(),
            "portal site test: nothing in site.json for {{colour}}"
        );
    }

    #[test]
    fn keys_are_filled_escaped_and_other_braces_left() {
        let site = built(
            &[(
                "a.html",
                "{{name}}|{{title}}|{{description}}|{{connect_src}}|{{{name}}}|{x}|{{ name }}",
            )],
            &[],
            None,
        );
        assert_eq!(
            body(&site, "a.html"),
            "Pool &amp; &quot;Co&quot;|Pool &amp; &quot;Co&quot;|&lt;b&gt;mining&lt;/b&gt;|'self'\
             |{Pool &amp; &quot;Co&quot;}|{x}|{{ name }}"
        );
        let mut titled = site_json(None);
        titled.title = Some("Home".to_owned());
        let site = assemble(
            sources(&[("a.html", "{{title}}")]),
            BTreeMap::new(),
            &titled,
        )
        .unwrap();
        assert_eq!(body(&site, "a.html"), "Home");
    }

    #[test]
    fn line_ends_are_read_as_python_reads_them() {
        let site = built(&[("a.html", "a\r\nb\rc\n"), ("b.png", "a\r\nb")], &[], None);
        assert_eq!(body(&site, "a.html"), "a\nb\nc\n");
        assert_eq!(body(&site, "b.png"), "a\r\nb", "only pages are text");
    }

    #[test]
    fn an_overlay_replaces_files_and_partials_and_adds_its_own() {
        let site = built(
            &[
                ("index.html", "<!--#include intro.html -->\n"),
                ("partials/intro.html", ""),
                ("logo.png", "old"),
            ],
            &[
                ("partials/intro.html", "Welcome"),
                ("logo.png", "new"),
                ("about.html", "{{name}}"),
                ("extra/data.json", "{}"),
            ],
            None,
        );
        assert_eq!(body(&site, "index.html"), "Welcome\n");
        assert_eq!(body(&site, "logo.png"), "new");
        assert_eq!(body(&site, "about.html"), "Pool &amp; &quot;Co&quot;");
        assert_eq!(
            site.files["extra/data.json"].content_type,
            "application/json"
        );
    }

    #[test]
    fn a_page_gets_its_canonical_link_where_the_site_names_its_origin() {
        let files = [
            ("index.html", "<head></head>"),
            ("stats.html", "<head></head></head>"),
            ("sub/index.html", "<head></head>"),
            ("404.html", "<head></head>"),
            (
                "fleet.html",
                "<head><meta name=\"robots\" content=\"noindex\"></head>",
            ),
        ];
        let site = built(&files, &[], Some("https://pool.example.com"));
        let link = |address: &str| {
            format!(
                "<head><link rel=\"canonical\" href=\"https://pool.example.com{address}\">\n</head>"
            )
        };
        assert_eq!(body(&site, "index.html"), link("/"));
        assert_eq!(
            body(&site, "stats.html"),
            format!("{}</head>", link("/stats.html"))
        );
        assert_eq!(body(&site, "sub/index.html"), link("/sub/"));
        assert_eq!(body(&site, "404.html"), "<head></head>");
        assert!(!body(&site, "fleet.html").contains("canonical"));
        let site = built(&files, &[], None);
        assert_eq!(body(&site, "index.html"), "<head></head>");
    }

    #[test]
    fn config_js_names_the_sites_own_origin() {
        let site = built(&[("config.js", "export const API = \"x\";")], &[], None);
        assert_eq!(body(&site, "config.js"), CONFIG_JS);
        assert!(CONFIG_JS.ends_with("export const API = \"\";\n"));
    }

    #[test]
    fn site_json_needs_a_name_and_a_description_and_ignores_the_api() {
        let site = parse_site(
            r#"{"name": "N", "description": "D", "api": "https://api.example.com", "origin": "https://a.example/"}"#,
        )
        .unwrap();
        assert_eq!(site.origin.as_deref(), Some("https://a.example"));
        assert!(parse_site(r#"{"name": "N"}"#).is_err());
        assert!(parse_site(r#"{"name": "N", "description": "D", "origin": "a.example"}"#).is_err());
        assert!(parse_site(r#"{"name": "N", "description": "D", "origin": ""}"#).is_ok());
    }

    #[test]
    fn origins_are_checked_as_build_py_checks_them() {
        for good in [
            "https://pool.example.com",
            "http://192.0.2.5:8080",
            "https://[::1]:8443",
            "http://localhost",
        ] {
            assert!(is_origin(good), "{good}");
        }
        for bad in [
            "pool.example.com",
            "https://",
            "https://pool.example.com/path",
            "https://pool.example.com:",
            "https://user@pool.example.com",
            "ftp://pool.example.com",
            "https://[]",
            "https://[::1",
        ] {
            assert!(!is_origin(bad), "{bad}");
        }
    }

    #[test]
    fn each_file_goes_with_its_type_cache_and_headers() {
        let site = built(
            &[
                ("index.html", "home"),
                ("404.html", "missing"),
                ("js/app.js", ""),
                ("vendor/lib-1.0.js", ""),
            ],
            &[],
            None,
        );
        let get = |path: &str| site.response(&path.parse::<Uri>().unwrap());
        let home = get("/");
        assert_eq!(home.status(), StatusCode::OK);
        assert_eq!(
            home.headers()[header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        assert_eq!(home.headers()[header::CACHE_CONTROL], "no-cache");
        for (name, value) in HEADERS {
            assert_eq!(home.headers()[name], value);
        }
        assert_eq!(get("/index.html?x=1").status(), StatusCode::OK);
        let script = get("/js/app.js");
        assert_eq!(
            script.headers()[header::CONTENT_TYPE],
            "text/javascript; charset=utf-8"
        );
        assert!(
            get("/vendor/lib-1.0.js").headers()[header::CACHE_CONTROL]
                .to_str()
                .unwrap()
                .contains("immutable")
        );
        let missing = get("/no/such/page");
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            missing.headers()[header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        assert_eq!(get("/partials/head.html").status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn the_embedded_app_builds_as_puntohunter_and_an_unreadable_site_is_named() {
        let site = Site::build(None, None).unwrap();
        assert_eq!(site.name(), "PuntoHunter");
        for path in [
            "index.html",
            "fleet.html",
            "404.html",
            "config.js",
            "app.css",
        ] {
            assert!(site.files.contains_key(path), "{path}");
        }
        assert!(body(&site, "index.html").contains("<title>PuntoHunter</title>"));
        let err = Site::build(Some(Path::new("/nonexistent/site.json")), None)
            .err()
            .unwrap();
        assert!(
            err.to_string()
                .starts_with("portal site /nonexistent/site.json: ")
        );
        let err = Site::build(None, Some(Path::new("/nonexistent/overlay")))
            .err()
            .unwrap();
        assert!(
            err.to_string()
                .starts_with("portal site /nonexistent/overlay: ")
        );
    }

    #[test]
    fn an_overlay_directory_is_read_with_its_subdirectories() {
        let dir = std::env::temp_dir().join(format!("puntohunter-overlay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("partials")).unwrap();
        std::fs::write(dir.join("partials/intro.html"), "Hello from {{name}}").unwrap();
        std::fs::write(dir.join("logo.png"), "png").unwrap();
        let site = Site::build(None, Some(&dir)).unwrap();
        assert!(body(&site, "index.html").contains("Hello from PuntoHunter"));
        assert_eq!(body(&site, "logo.png"), "png");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_vendored_files_match_their_recorded_checksums() {
        let sums = App::get("vendor/SHA256SUMS").expect("recorded checksums");
        let sums = std::str::from_utf8(&sums.data).unwrap();
        let mut checked = 0;
        for line in sums.lines() {
            let (sum, name) = line.split_once("  ").unwrap();
            let file = App::get(&format!("vendor/{name}")).unwrap_or_else(|| panic!("{name}"));
            assert_eq!(sha256::Hash::hash(&file.data).to_string(), sum, "{name}");
            checked += 1;
        }
        let vendored = App::iter()
            .filter(|path| path.starts_with("vendor/") && *path != "vendor/SHA256SUMS")
            .count();
        assert_eq!(checked, vendored, "every vendored file has a checksum");
    }
}
