//! The fixture matrix: every "hard case" the goal lists, built deterministically.

use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
use hazar_engine::MediaKind;

use crate::server::Route;

type Aes128Cbc = cbc::Encryptor<aes::Aes128>;

pub const KEY: [u8; 16] = [
    0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
];
pub const IV: [u8; 16] = [
    0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90, 0xa0, 0xb0, 0xc0, 0xd0, 0xe0, 0xf0, 0x01,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expectation {
    /// Detect and download; output bytes must match `expected_bytes`.
    Download { kind: MediaKind },
    /// Detected but deliberately not downloaded (DASH, DRM).
    DetectOnly {
        kind: MediaKind,
        drm: Option<&'static str>,
    },
    /// Download must fail cleanly with this text in the error.
    Refused { contains: &'static str },
}

#[derive(Debug, Clone)]
pub struct Case {
    pub name: &'static str,
    pub page_path: String,
    pub expectation: Expectation,
    /// Extra headers handed to both the resolver and the engine (cookies, …).
    pub headers: Vec<(String, String)>,
    pub expected_bytes: Vec<u8>,
    pub note: &'static str,
    /// Strategy expected to produce the winning candidate (documentation).
    pub strategy: &'static str,
}

pub struct Fixtures {
    pub routes: Vec<Route>,
    pub cases: Vec<Case>,
}

fn blob(seed: u8, len: usize) -> Vec<u8> {
    let mut state = 0x9e37_79b9_7f4a_7c15u64 ^ (seed as u64).wrapping_mul(0x2545_f491_4f6c_dd1d);
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.push((state & 0xff) as u8);
    }
    out
}

/// Deterministic payload for the browser layer too.
pub fn blob_public(seed: u8, len: usize) -> Vec<u8> {
    blob(seed, len)
}

fn encrypt(data: &[u8]) -> Vec<u8> {
    let encryptor = Aes128Cbc::new_from_slices(&KEY, &IV).expect("key/iv");
    encryptor.encrypt_padded_vec_mut::<Pkcs7>(data)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let buffer = ((chunk[0] as u32) << 16)
            | ((chunk.get(1).copied().unwrap_or(0) as u32) << 8)
            | chunk.get(2).copied().unwrap_or(0) as u32;
        out.push(TABLE[((buffer >> 18) & 63) as usize] as char);
        out.push(TABLE[((buffer >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((buffer >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(buffer & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn hex_escaped(data: &[u8]) -> String {
    data.iter().map(|byte| format!("\\x{byte:02x}")).collect()
}

#[derive(Debug, Clone, Copy, Default)]
pub struct HlsOpts {
    pub segments: usize,
    pub master: bool,
    pub encrypted: bool,
    pub byte_range: bool,
    pub init_map: bool,
    pub live: bool,
    pub sample_aes: bool,
    pub expiring: bool,
    pub truncate_first_segment: bool,
}

pub struct Hls {
    pub routes: Vec<Route>,
    pub manifest_path: String,
    pub output: Vec<u8>,
}

/// Build an HLS ladder: playlist(s) + segment routes + the expected assembled bytes.
pub fn hls(prefix: &str, opts: HlsOpts) -> Hls {
    let count = opts.segments.max(1);
    let segment_size = 16 * 1024;
    let segments: Vec<Vec<u8>> = (0..count)
        .map(|index| blob((index + 1) as u8, segment_size))
        .collect();
    let init = opts.init_map.then(|| blob(9, 4096));

    let mut output = Vec::new();
    if let Some(init) = &init {
        output.extend_from_slice(init);
    }
    for segment in &segments {
        output.extend_from_slice(segment);
    }

    let suffix = if opts.expiring { "?t=abc123" } else { "" };
    let mut routes = Vec::new();
    let mut playlist = String::from("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:6\n");

    if opts.encrypted || opts.sample_aes {
        routes.push(Route::file(format!("{prefix}/key.bin"), KEY.to_vec()));
    }
    if opts.encrypted {
        playlist.push_str(&format!(
            "#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x{}\n",
            hex(&IV)
        ));
    }
    if opts.sample_aes {
        playlist.push_str("#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n");
    }

    if opts.byte_range {
        let mut resource = Vec::new();
        if let Some(init) = &init {
            resource.extend_from_slice(init);
        }
        for segment in &segments {
            resource.extend_from_slice(segment);
        }
        routes.push(
            Route::file(format!("{prefix}/data.m4s"), resource).tweak(|route| {
                route.require_query = opts.expiring.then(|| "t=".to_string());
            }),
        );

        let mut offset = 0usize;
        if let Some(init) = &init {
            playlist.push_str(&format!(
                "#EXT-X-MAP:URI=\"data.m4s{suffix}\",BYTERANGE=\"{}@{offset}\"\n",
                init.len()
            ));
            offset += init.len();
        }
        for segment in &segments {
            playlist.push_str(&format!(
                "#EXTINF:6.0,\n#EXT-X-BYTERANGE:{0}@{1}\n{prefix}/data.m4s{suffix}\n",
                segment.len(),
                offset
            ));
            offset += segment.len();
        }
    } else {
        if let Some(init) = &init {
            playlist.push_str(&format!("#EXT-X-MAP:URI=\"init.mp4{suffix}\"\n"));
            routes.push(
                Route::file(format!("{prefix}/init.mp4"), init.clone()).tweak(|route| {
                    route.require_query = opts.expiring.then(|| "t=".to_string());
                }),
            );
        }
        for (index, segment) in segments.iter().enumerate() {
            let body = if opts.encrypted {
                encrypt(segment)
            } else {
                segment.clone()
            };
            let truncate = opts.truncate_first_segment && index == 0;
            routes.push(
                Route::file(format!("{prefix}/seg{index}.ts"), body).tweak(|route| {
                    route.require_query = opts.expiring.then(|| "t=".to_string());
                    if truncate {
                        route.truncate_first = 1;
                    }
                }),
            );
            playlist.push_str(&format!("#EXTINF:6.0,\n{prefix}/seg{index}.ts{suffix}\n"));
        }
    }

    if !opts.live {
        playlist.push_str("#EXT-X-ENDLIST\n");
    }

    let manifest_path = format!("{prefix}/index.m3u8");
    if opts.master {
        let low = format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\n{prefix}/seg0.ts{suffix}\n#EXT-X-ENDLIST\n"
        );
        routes.push(Route::playlist(format!("{prefix}/low.m3u8"), low));
        routes.push(Route::playlist(format!("{prefix}/high.m3u8"), playlist));
        routes.push(Route::playlist(
            manifest_path.clone(),
            format!(
                "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\nlow.m3u8\n\
                 #EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080\nhigh.m3u8\n"
            ),
        ));
    } else {
        routes.push(Route::playlist(manifest_path.clone(), playlist));
    }

    Hls {
        routes,
        manifest_path,
        output,
    }
}

/// Dean Edwards packed blob whose payload references the dictionary entry at
/// index 10 (base-36 `a`). The dictionary holds the relative manifest path, so
/// nothing has to know the fixture's host or port.
fn packed_js(path: &str) -> String {
    let mut words: Vec<String> = (0..10).map(|index| format!("w{index}")).collect();
    words.push(path.to_string());
    words.push("unused".to_string());
    format!(
        "eval(function(p,a,c,k,e,d){{e=function(c){{return c.toString(36)}};if(!''.replace(/^/,String)){{while(c--){{d[e(c)]=k[c]||e(c)}}k=[function(e){{return d[e]}}];e=function(){{return'\\\\w+'}};c=1}};while(c--){{if(k[c]){{p=p.replace(new RegExp('\\\\b'+e(c)+'\\\\b','g'),k[c])}}}}return p}}('var u=\"a\";',36,12,'{}'.split('|'),0,{{}}))",
        words.join("|")
    )
}

fn dash_manifest(prefix: &str, widevine: bool) -> String {
    let protection = if widevine {
        "<ContentProtection schemeIdUri=\"urn:uuid:EDEF8BA9-79D6-4ACE-A3C8-27DCD51D21ED\"/>"
    } else {
        ""
    };
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT30S" profiles="urn:mpeg:dash:profile:isoff-on-demand:2011">
  <Period>
    <AdaptationSet mimeType="video/mp4" segmentAlignment="true">
      {protection}
      <Representation id="v0" bandwidth="2400000" codecs="avc1.64001f" width="1920" height="1080">
        <SegmentTemplate timescale="1000" duration="6000" initialization="{prefix}/init.mp4" media="{prefix}/seg-$Number$.m4s" startNumber="1"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>
"#
    )
}

/// The whole matrix.
pub fn build() -> Fixtures {
    let mut routes: Vec<Route> = Vec::new();
    let mut cases: Vec<Case> = Vec::new();

    // ---------------------------------------------------------------- files
    {
        let body = blob(1, 256 * 1024);
        routes.push(Route::file("/file-direct/file.bin", body.clone()));
        routes.push(Route::html(
            "/file-direct/page.html",
            "<html><body><video src=\"/file-direct/file.bin\"></video></body></html>".to_string(),
        ));
        cases.push(Case {
            name: "file-direct-ranges",
            page_path: "/file-direct/page.html".to_string(),
            expectation: Expectation::Download {
                kind: MediaKind::File,
            },
            headers: Vec::new(),
            expected_bytes: body,
            note: "range destekli doğrudan dosya",
            strategy: "attribute-scan",
        });
    }
    {
        let body = blob(2, 192 * 1024);
        routes.push(
            Route::file("/file-no-ranges/file.bin", body.clone()).tweak(|route| route.ranges = false),
        );
        routes.push(Route::html(
            "/file-no-ranges/page.html",
            "<html><body><a href=\"/file-no-ranges/file.bin\">download</a></body></html>".to_string(),
        ));
        cases.push(Case {
            name: "file-no-ranges",
            page_path: "/file-no-ranges/page.html".to_string(),
            expectation: Expectation::Download {
                kind: MediaKind::File,
            },
            headers: Vec::new(),
            expected_bytes: body,
            note: "Accept-Ranges yok → tek connection",
            strategy: "attribute-scan",
        });
    }
    {
        let body = blob(3, 128 * 1024);
        routes.push(Route::file("/file-redirect/file.bin", body.clone()));
        routes.push(Route::redirect("/file-redirect/hop1", "/file-redirect/hop2"));
        routes.push(Route::redirect("/file-redirect/hop2", "/file-redirect/file.bin"));
        routes.push(Route::html(
            "/file-redirect/page.html",
            "<html><body><video src=\"/file-redirect/hop1\"></video></body></html>".to_string(),
        ));
        cases.push(Case {
            name: "file-redirect-chain",
            page_path: "/file-redirect/page.html".to_string(),
            expectation: Expectation::Download {
                kind: MediaKind::File,
            },
            headers: Vec::new(),
            expected_bytes: body,
            note: "302 zinciri (redirect takibi)",
            strategy: "attribute-scan",
        });
    }
    {
        let body = blob(4, 128 * 1024);
        routes.push(
            Route::file("/file-cookie-gate/file.bin", body.clone())
                .tweak(|route| route.require_cookie = Some("sid=session1".to_string())),
        );
        routes.push(Route::html(
            "/file-cookie-gate/page.html",
            "<html><body><video src=\"/file-cookie-gate/file.bin\"></video></body></html>"
                .to_string(),
        ));
        cases.push(Case {
            name: "file-cookie-gate",
            page_path: "/file-cookie-gate/page.html".to_string(),
            expectation: Expectation::Download {
                kind: MediaKind::File,
            },
            headers: vec![("Cookie".to_string(), "sid=session1".to_string())],
            expected_bytes: body,
            note: "cookie olmadan 403 (session'lı dosya)",
            strategy: "attribute-scan",
        });
    }
    {
        let body = blob(5, 128 * 1024);
        routes.push(
            Route::file("/file-referer-gate/file.bin", body.clone())
                .tweak(|route| route.require_referer = true),
        );
        routes.push(Route::html(
            "/file-referer-gate/page.html",
            "<html><body><video src=\"/file-referer-gate/file.bin\"></video></body></html>"
                .to_string(),
        ));
        cases.push(Case {
            name: "file-referer-gate",
            page_path: "/file-referer-gate/page.html".to_string(),
            expectation: Expectation::Download {
                kind: MediaKind::File,
            },
            headers: Vec::new(),
            expected_bytes: body,
            note: "hotlink koruması → Referer otomatik eklenir",
            strategy: "attribute-scan",
        });
    }
    {
        let body = blob(6, 96 * 1024);
        routes.push(
            Route::file("/file-rate-limit/file.bin", body.clone())
                .tweak(|route| route.rate_limit_first = 2),
        );
        routes.push(Route::html(
            "/file-rate-limit/page.html",
            "<html><body><video src=\"/file-rate-limit/file.bin\"></video></body></html>"
                .to_string(),
        ));
        cases.push(Case {
            name: "file-rate-limit-429",
            page_path: "/file-rate-limit/page.html".to_string(),
            expectation: Expectation::Download {
                kind: MediaKind::File,
            },
            headers: Vec::new(),
            expected_bytes: body,
            note: "ilk 2 istek 429 + Retry-After → backoff ile başarı",
            strategy: "attribute-scan",
        });
    }

    {
        let body = blob(7, 96 * 1024);
        routes.push(
            Route::file("/file-503/file.bin", body.clone()).tweak(|route| {
                route.rate_limit_first = 1;
                route.rate_limit_status = 503;
            }),
        );
        routes.push(Route::html(
            "/file-503/page.html",
            "<html><body><video src=\"/file-503/file.bin\"></video></body></html>".to_string(),
        ));
        cases.push(Case {
            name: "file-retry-after-503",
            page_path: "/file-503/page.html".to_string(),
            expectation: Expectation::Download {
                kind: MediaKind::File,
            },
            headers: Vec::new(),
            expected_bytes: body,
            note: "503 + Retry-After → backoff ile başarı",
            strategy: "attribute-scan",
        });
    }

    {
        // Preroll/ad stream and the real episode stream on the same page: the ad
        // must be skipped (URL pattern), the episode must be downloaded.
        let ad = hls(
            "/ad-vs-main/preroll/ads",
            HlsOpts {
                segments: 1,
                ..Default::default()
            },
        );
        let main = hls(
            "/ad-vs-main",
            HlsOpts {
                segments: 6,
                ..Default::default()
            },
        );
        routes.extend(ad.routes);
        routes.extend(main.routes);
        routes.push(Route::html(
            "/ad-vs-main/page.html",
            format!(
                "<html><body><script>jwplayer(\"p\").setup({{sources:[{{file:\"{}\"}}]}});</script><video src=\"{}\"></video></body></html>",
                ad.manifest_path, main.manifest_path
            ),
        ));
        cases.push(Case {
            name: "ad-vs-main-stream",
            page_path: "/ad-vs-main/page.html".to_string(),
            expectation: Expectation::Download { kind: MediaKind::Hls },
            headers: Vec::new(),
            expected_bytes: main.output,
            note: "reklam/preroll stream'i atlanır, gerçek bölüm indirilir",
            strategy: "attribute-scan",
        });
    }

    // ------------------------------------------------------------------ hls
    {
        let ladder = hls("/hls-plain", HlsOpts { segments: 3, ..Default::default() });
        routes.extend(ladder.routes);
        routes.push(Route::html(
            "/hls-plain/page.html",
            format!(
                "<html><body><video controls src=\"{}\"></video></body></html>",
                ladder.manifest_path
            ),
        ));
        cases.push(Case {
            name: "hls-plain-video-tag",
            page_path: "/hls-plain/page.html".to_string(),
            expectation: Expectation::Download { kind: MediaKind::Hls },
            headers: Vec::new(),
            expected_bytes: ladder.output,
            note: "<video src> içinde düz HLS",
            strategy: "attribute-scan",
        });
    }
    {
        let ladder = hls(
            "/hls-master",
            HlsOpts {
                segments: 3,
                master: true,
                ..Default::default()
            },
        );
        routes.extend(ladder.routes);
        routes.push(Route::html(
            "/hls-master/page.html",
            format!(
                "<html><body><video><source src=\"{}\" type=\"application/vnd.apple.mpegurl\"></video></body></html>",
                ladder.manifest_path
            ),
        ));
        cases.push(Case {
            name: "hls-master-best-variant",
            page_path: "/hls-master/page.html".to_string(),
            expectation: Expectation::Download { kind: MediaKind::Hls },
            headers: Vec::new(),
            expected_bytes: ladder.output,
            note: "master playlist → en yüksek BANDWIDTH varyantı",
            strategy: "manifest-enum",
        });
    }
    {
        let ladder = hls(
            "/hls-aes",
            HlsOpts {
                segments: 3,
                encrypted: true,
                ..Default::default()
            },
        );
        routes.extend(ladder.routes);
        routes.push(Route::html(
            "/hls-aes/page.html",
            format!(
                "<html><body><video src=\"{}\"></video></body></html>",
                ladder.manifest_path
            ),
        ));
        cases.push(Case {
            name: "hls-aes128",
            page_path: "/hls-aes/page.html".to_string(),
            expectation: Expectation::Download { kind: MediaKind::Hls },
            headers: Vec::new(),
            expected_bytes: ladder.output,
            note: "EXT-X-KEY AES-128 + IV",
            strategy: "attribute-scan",
        });
    }
    {
        let ladder = hls(
            "/hls-byterange",
            HlsOpts {
                segments: 3,
                byte_range: true,
                init_map: true,
                ..Default::default()
            },
        );
        routes.extend(ladder.routes);
        routes.push(Route::html(
            "/hls-byterange/page.html",
            format!(
                "<html><body><video src=\"{}\"></video></body></html>",
                ladder.manifest_path
            ),
        ));
        cases.push(Case {
            name: "hls-map-byterange",
            page_path: "/hls-byterange/page.html".to_string(),
            expectation: Expectation::Download { kind: MediaKind::Hls },
            headers: Vec::new(),
            expected_bytes: ladder.output,
            note: "EXT-X-MAP + EXT-X-BYTERANGE (tek kaynak üzerinden parçalar)",
            strategy: "attribute-scan",
        });
    }
    {
        let ladder = hls(
            "/hls-token",
            HlsOpts {
                segments: 3,
                expiring: true,
                ..Default::default()
            },
        );
        routes.extend(ladder.routes);
        routes.push(Route::html(
            "/hls-token/page.html",
            format!(
                "<html><body><video src=\"{}\"></video></body></html>",
                ladder.manifest_path
            ),
        ));
        cases.push(Case {
            name: "hls-expiring-token",
            page_path: "/hls-token/page.html".to_string(),
            expectation: Expectation::Download { kind: MediaKind::Hls },
            headers: Vec::new(),
            expected_bytes: ladder.output,
            note: "segment URL'leri süreli token taşıyor",
            strategy: "attribute-scan",
        });
    }
    {
        let ladder = hls(
            "/hls-live",
            HlsOpts {
                segments: 3,
                live: true,
                ..Default::default()
            },
        );
        routes.extend(ladder.routes);
        routes.push(Route::html(
            "/hls-live/page.html",
            format!(
                "<html><body><video src=\"{}\"></video></body></html>",
                ladder.manifest_path
            ),
        ));
        cases.push(Case {
            name: "hls-live-no-endlist",
            page_path: "/hls-live/page.html".to_string(),
            expectation: Expectation::Download { kind: MediaKind::Hls },
            headers: Vec::new(),
            expected_bytes: ladder.output,
            note: "ENDLIST yok (canlı playlist)",
            strategy: "attribute-scan",
        });
    }
    {
        let ladder = hls(
            "/hls-abort",
            HlsOpts {
                segments: 3,
                truncate_first_segment: true,
                ..Default::default()
            },
        );
        routes.extend(ladder.routes);
        routes.push(Route::html(
            "/hls-abort/page.html",
            format!(
                "<html><body><video src=\"{}\"></video></body></html>",
                ladder.manifest_path
            ),
        ));
        cases.push(Case {
            name: "hls-abort-mid-segment",
            page_path: "/hls-abort/page.html".to_string(),
            expectation: Expectation::Download { kind: MediaKind::Hls },
            headers: Vec::new(),
            expected_bytes: ladder.output,
            note: "segment indirilirken bağlantı kopuyor → retry",
            strategy: "attribute-scan",
        });
    }
    {
        let ladder = hls(
            "/hls-drm",
            HlsOpts {
                segments: 2,
                sample_aes: true,
                ..Default::default()
            },
        );
        routes.extend(ladder.routes);
        routes.push(Route::html(
            "/hls-drm/page.html",
            format!(
                "<html><body><video src=\"{}\"></video></body></html>",
                ladder.manifest_path
            ),
        ));
        cases.push(Case {
            name: "hls-sample-aes-refused",
            page_path: "/hls-drm/page.html".to_string(),
            expectation: Expectation::Refused {
                contains: "SAMPLE-AES",
            },
            headers: Vec::new(),
            expected_bytes: Vec::new(),
            note: "SAMPLE-AES (DRM) → net ret, kırma yok",
            strategy: "attribute-scan",
        });
    }

    // ----------------------------------------------------------------- dash
    {
        routes.push(Route::dash(
            "/dash-plain/manifest.mpd",
            dash_manifest("/dash-plain", false),
        ));
        routes.push(Route::html(
            "/dash-plain/page.html",
            "<html><body><video src=\"/dash-plain/manifest.mpd\"></video></body></html>".to_string(),
        ));
        cases.push(Case {
            name: "dash-segment-template",
            page_path: "/dash-plain/page.html".to_string(),
            expectation: Expectation::DetectOnly {
                kind: MediaKind::Dash,
                drm: None,
            },
            headers: Vec::new(),
            expected_bytes: Vec::new(),
            note: "DASH SegmentTemplate → tespit (indirme henüz yok)",
            strategy: "attribute-scan",
        });
    }
    {
        routes.push(Route::dash(
            "/dash-drm/manifest.mpd",
            dash_manifest("/dash-drm", true),
        ));
        routes.push(Route::html(
            "/dash-drm/page.html",
            "<html><body><video src=\"/dash-drm/manifest.mpd\"></video></body></html>".to_string(),
        ));
        cases.push(Case {
            name: "dash-widevine",
            page_path: "/dash-drm/page.html".to_string(),
            expectation: Expectation::DetectOnly {
                kind: MediaKind::Dash,
                drm: Some("widevine"),
            },
            headers: Vec::new(),
            expected_bytes: Vec::new(),
            note: "Widevine işaretli → tespit + drm raporu",
            strategy: "attribute-scan",
        });
    }

    // ------------------------------------------------- page/player shapes
    {
        let ladder = hls(
            "/html-og",
            HlsOpts {
                segments: 3,
                ..Default::default()
            },
        );
        routes.extend(ladder.routes);
        routes.push(Route::html(
            "/html-og/page.html",
            format!(
                "<html><head><meta property=\"og:video\" content=\"{}\"></head><body></body></html>",
                ladder.manifest_path
            ),
        ));
        cases.push(Case {
            name: "html-og-video",
            page_path: "/html-og/page.html".to_string(),
            expectation: Expectation::Download { kind: MediaKind::Hls },
            headers: Vec::new(),
            expected_bytes: ladder.output,
            note: "og:video meta etiketi",
            strategy: "attribute-scan",
        });
    }
    {
        let ladder = hls(
            "/html-jsonld",
            HlsOpts {
                segments: 3,
                ..Default::default()
            },
        );
        routes.extend(ladder.routes);
        routes.push(Route::html(
            "/html-jsonld/page.html",
            format!(
                "<html><head><script type=\"application/ld+json\">{{'@type':'VideoObject','contentUrl':'{}'}}</script></head><body></body></html>",
                ladder.manifest_path
            )
            .replace('\'', "\""),
        ));
        cases.push(Case {
            name: "html-jsonld",
            page_path: "/html-jsonld/page.html".to_string(),
            expectation: Expectation::Download { kind: MediaKind::Hls },
            headers: Vec::new(),
            expected_bytes: ladder.output,
            note: "JSON-LD contentUrl",
            strategy: "json-ld",
        });
    }
    {
        let ladder = hls(
            "/html-jw",
            HlsOpts {
                segments: 3,
                ..Default::default()
            },
        );
        routes.extend(ladder.routes);
        routes.push(Route::html(
            "/html-jw/page.html",
            format!(
                "<html><body><script>jwplayer(\"p\").setup({{sources:[{{file:\"{}\"}}]}});</script></body></html>",
                ladder.manifest_path
            ),
        ));
        cases.push(Case {
            name: "html-jwplayer-config",
            page_path: "/html-jw/page.html".to_string(),
            expectation: Expectation::Download { kind: MediaKind::Hls },
            headers: Vec::new(),
            expected_bytes: ladder.output,
            note: "jwplayer setup() config",
            strategy: "player-config",
        });
    }
    {
        let ladder = hls(
            "/html-vjs",
            HlsOpts {
                segments: 3,
                ..Default::default()
            },
        );
        routes.extend(ladder.routes);
        routes.push(Route::html(
            "/html-vjs/page.html",
            format!(
                "<html><body><script>var player = videojs(\"p\");player.src({{src:\"{}\",type:\"application/x-mpegURL\"}});</script></body></html>",
                ladder.manifest_path
            ),
        ));
        cases.push(Case {
            name: "html-videojs-config",
            page_path: "/html-vjs/page.html".to_string(),
            expectation: Expectation::Download { kind: MediaKind::Hls },
            headers: Vec::new(),
            expected_bytes: ladder.output,
            note: "videojs src() config",
            strategy: "player-config",
        });
    }
    {
        let ladder = hls(
            "/html-dp",
            HlsOpts {
                segments: 3,
                ..Default::default()
            },
        );
        routes.extend(ladder.routes);
        routes.push(Route::html(
            "/html-dp/page.html",
            format!(
                "<html><body><script>new DPlayer({{container:document.getElementById(\"p\"),video:{{url:\"{}\"}}}});</script></body></html>",
                ladder.manifest_path
            ),
        ));
        cases.push(Case {
            name: "html-dplayer-config",
            page_path: "/html-dp/page.html".to_string(),
            expectation: Expectation::Download { kind: MediaKind::Hls },
            headers: Vec::new(),
            expected_bytes: ladder.output,
            note: "DPlayer video.url config (pkplyr benzeri)",
            strategy: "player-config",
        });
    }
    {
        let ladder = hls(
            "/html-iframe",
            HlsOpts {
                segments: 3,
                ..Default::default()
            },
        );
        routes.extend(ladder.routes);
        routes.push(Route::html(
            "/html-iframe/player.html",
            format!(
                "<html><body><video src=\"{}\"></video></body></html>",
                ladder.manifest_path
            ),
        ));
        routes.push(Route::html(
            "/html-iframe/page.html",
            "<html><body><iframe src=\"/html-iframe/player.html\" allowfullscreen></iframe></body></html>"
                .to_string(),
        ));
        cases.push(Case {
            name: "html-iframe-player",
            page_path: "/html-iframe/page.html".to_string(),
            expectation: Expectation::Download { kind: MediaKind::Hls },
            headers: Vec::new(),
            expected_bytes: ladder.output,
            note: "oynatıcı iframe içinde → özyineleme",
            strategy: "iframe",
        });
    }
    {
        // pkplyr / playerjs style: the sources live in an external config file.
        let ladder = hls(
            "/html-pkplyr",
            HlsOpts {
                segments: 3,
                ..Default::default()
            },
        );
        routes.extend(ladder.routes);
        routes.push(Route::html(
            "/html-pkplyr/page.html",
            "<html><body><script src=\"/html-pkplyr/player.js\"></script></body></html>".to_string(),
        ));
        routes.push(Route::new(
            "/html-pkplyr/player.js",
            "application/javascript",
            format!(
                "var pkplyrConfig={{\"sources\":[{{\"file\":\"{}\",\"label\":\"1080p\"}}],\"autostart\":true}};",
                ladder.manifest_path
            )
            .into_bytes(),
        ));
        cases.push(Case {
            name: "html-pkplyr-config",
            page_path: "/html-pkplyr/page.html".to_string(),
            expectation: Expectation::Download { kind: MediaKind::Hls },
            headers: Vec::new(),
            expected_bytes: ladder.output,
            note: "harici player.js içinde pkplyr/playerjs sources config",
            strategy: "player-config",
        });
    }

    {
        // Alternatif kaynaklar: ilk mirror ölü, ikincisi çalışıyor.
        let dead = blob(51, 8 * 1024);
        let alive = blob(52, 24 * 1024);
        routes.push(
            Route::file("/alternate/1080p.mp4", dead).tweak(|route| {
                route.require_cookie = Some("never=set".to_string());
            }),
        );
        routes.push(Route::file("/alternate/720p.mp4", alive.clone()));
        routes.push(Route::html(
            "/alternate/page.html",
            "<html><body><script>var cfg={\"sources\":[{\"file\":\"/alternate/1080p.mp4\"},{\"file\":\"/alternate/720p.mp4\"}]};</script></body></html>"
                .to_string(),
        ));
        cases.push(Case {
            name: "alternate-sources-fallback",
            page_path: "/alternate/page.html".to_string(),
            expectation: Expectation::Download {
                kind: MediaKind::File,
            },
            headers: Vec::new(),
            expected_bytes: alive,
            note: "ölü mirror → sonraki kaynağa düşer",
            strategy: "player-config",
        });
    }

    {
        let ladder = hls(
            "/js-packed",
            HlsOpts {
                segments: 3,
                ..Default::default()
            },
        );
        routes.extend(ladder.routes);
        routes.push(Route::html(
            "/js-packed/page.html",
            "<html><body><script src=\"/js-packed/player.js\"></script></body></html>".to_string(),
        ));
        routes.push(Route::new(
            "/js-packed/player.js",
            "application/javascript",
            packed_js(&ladder.manifest_path).into_bytes(),
        ));
        cases.push(Case {
            name: "js-eval-packed",
            page_path: "/js-packed/page.html".to_string(),
            expectation: Expectation::Download { kind: MediaKind::Hls },
            headers: Vec::new(),
            expected_bytes: ladder.output,
            note: "eval-packed player JS",
            strategy: "js-unpack",
        });
    }
    {
        let ladder = hls(
            "/js-base64",
            HlsOpts {
                segments: 3,
                ..Default::default()
            },
        );
        routes.extend(ladder.routes);
        routes.push(Route::html(
            "/js-base64/page.html",
            format!(
                "<html><body><script>var e=\"{}\";var u=atob(e);</script></body></html>",
                base64(ladder.manifest_path.as_bytes())
            ),
        ));
        cases.push(Case {
            name: "js-base64-manifest",
            page_path: "/js-base64/page.html".to_string(),
            expectation: Expectation::Download { kind: MediaKind::Hls },
            headers: Vec::new(),
            expected_bytes: ladder.output,
            note: "base64 ile gizlenmiş manifest",
            strategy: "base64-decode",
        });
    }
    {
        let ladder = hls(
            "/js-hex",
            HlsOpts {
                segments: 3,
                ..Default::default()
            },
        );
        routes.extend(ladder.routes);
        routes.push(Route::html(
            "/js-hex/page.html",
            format!(
                "<html><body><script>var u=\"{}\";</script></body></html>",
                hex_escaped(ladder.manifest_path.as_bytes())
            ),
        ));
        cases.push(Case {
            name: "js-hex-escaped",
            page_path: "/js-hex/page.html".to_string(),
            expectation: Expectation::Download { kind: MediaKind::Hls },
            headers: Vec::new(),
            expected_bytes: ladder.output,
            note: "\\xNN escape ile gizlenmiş manifest",
            strategy: "js-unpack",
        });
    }

    // ------------------------------------------------------- segments-only
    {
        let segments: Vec<Vec<u8>> = (0..3).map(|index| blob(30 + index as u8, 12 * 1024)).collect();
        let mut output = Vec::new();
        for segment in &segments {
            output.extend_from_slice(segment);
        }
        for (index, segment) in segments.iter().enumerate() {
            routes.push(Route::file(
                format!("/segments-only/seg{index}.ts"),
                segment.clone(),
            ));
        }
        routes.push(Route::html(
            "/segments-only/page.html",
            "<html><body><script>var sources=[\"/segments-only/seg0.ts\",\"/segments-only/seg1.ts\",\"/segments-only/seg2.ts\"];</script></body></html>"
                .to_string(),
        ));
        cases.push(Case {
            name: "segments-only-mse",
            page_path: "/segments-only/page.html".to_string(),
            expectation: Expectation::Download { kind: MediaKind::Hls },
            headers: Vec::new(),
            expected_bytes: output,
            note: "manifest yok, yalnız segment URL'leri (MSE/blob)",
            strategy: "segment-group",
        });
    }

    // --------------------------------------------------- multi source pick
    {
        let low = blob(41, 10 * 1024);
        let high = blob(42, 40 * 1024);
        routes.push(Route::file("/multi-source/720p.mp4", low));
        routes.push(Route::file("/multi-source/1080p.mp4", high.clone()));
        routes.push(Route::html(
            "/multi-source/page.html",
            "<html><body><video><source src=\"/multi-source/720p.mp4\"></video>\
             <script>jwplayer(\"p\").setup({sources:[{file:\"/multi-source/1080p.mp4\"}]});</script></body></html>"
                .to_string(),
        ));
        cases.push(Case {
            name: "multi-source-pick-best",
            page_path: "/multi-source/page.html".to_string(),
            expectation: Expectation::Download {
                kind: MediaKind::File,
            },
            headers: Vec::new(),
            expected_bytes: high,
            note: "birkaç kaynak → player config'teki (daha güvenilir) seçilir",
            strategy: "player-config",
        });
    }

    Fixtures { routes, cases }
}

/// Site-shaped pages for the live-layer self-test: same profile definitions and
/// assertion code as the real run, but served locally.
pub fn live_like_routes() -> Vec<Route> {
    let mut routes: Vec<Route> = Vec::new();

    // hdfilmcehennemi: player lives inside an iframe.
    {
        let ladder = hls("/live/hdfilmcehennemi", HlsOpts { segments: 3, ..Default::default() });
        routes.extend(ladder.routes);
        routes.push(Route::html(
            "/live/hdfilmcehennemi/player.html",
            format!(
                "<html><body><video controls src=\"{}\"></video></body></html>",
                ladder.manifest_path
            ),
        ));
        routes.push(Route::html(
            "/live/hdfilmcehennemi/page.html",
            "<html><body><iframe src=\"/live/hdfilmcehennemi/player.html\" allowfullscreen></iframe></body></html>"
                .to_string(),
        ));
    }

    // dizipal: player config in an external file + expiring token segments.
    {
        let ladder = hls(
            "/live/dizipal",
            HlsOpts {
                segments: 3,
                expiring: true,
                ..Default::default()
            },
        );
        routes.extend(ladder.routes);
        routes.push(Route::html(
            "/live/dizipal/page.html",
            "<html><body><script src=\"/live/dizipal/player.js\"></script></body></html>".to_string(),
        ));
        routes.push(Route::new(
            "/live/dizipal/player.js",
            "application/javascript",
            format!("var cfg={{\"sources\":[{{\"file\":\"{}\"}}]}};", ladder.manifest_path)
                .into_bytes(),
        ));
    }

    // youtube-like: DASH manifest (no cipher handling anywhere in the project).
    {
        routes.push(Route::dash("/live/youtube/manifest.mpd", dash_manifest("/live/youtube", false)));
        routes.push(Route::html(
            "/live/youtube/page.html",
            "<html><body><video src=\"/live/youtube/manifest.mpd\"></video></body></html>".to_string(),
        ));
    }

    // dailymotion-like: inline player config.
    {
        let ladder = hls("/live/dailymotion", HlsOpts { segments: 3, ..Default::default() });
        routes.extend(ladder.routes);
        routes.push(Route::html(
            "/live/dailymotion/page.html",
            format!(
                "<html><body><script>jwplayer(\"p\").setup({{sources:[{{file:\"{}\"}}]}});</script></body></html>",
                ladder.manifest_path
            ),
        ));
    }

    // js-only: the manifest URL is only assembled inside the page's script, so
    // the plain HTTP resolver cannot see it — a real browser is required.
    {
        let ladder = hls(
            "/live/js-only",
            HlsOpts {
                segments: 3,
                ..Default::default()
            },
        );
        routes.extend(ladder.routes);
        routes.push(Route::html(
            "/live/js-only/page.html",
            "<html><body><video controls></video><script src=\"/live/js-only/player.js\"></script></body></html>"
                .to_string(),
        ));
        routes.push(Route::new(
            "/live/js-only/player.js",
            "application/javascript",
            r#"(async function () {
                var p = ['/live', '/js-only', '/index', '.m3u8'].join('');
                var r = await fetch(p);
                var t = await r.text();
                var urls = t.split(String.fromCharCode(10)).map(function (l) { return l.trim(); }).filter(function (l) { return l && l[0] !== '#'; });
                for (var i = 0; i < Math.min(2, urls.length); i++) {
                    try { await fetch(new URL(urls[i], location.href).toString()); } catch (e) {}
                }
            })();
"#
            .to_string()
            .into_bytes(),
        ));
    }

    // iframe + expiring-token HLS (hdfilmcehennemi / dizipal şekli), manifest
    // URL'i iframe içindeki script tarafından runtime'da kuruluyor.
    {
        let ladder = hls(
            "/live/browser-iframe",
            HlsOpts {
                segments: 3,
                expiring: true,
                ..Default::default()
            },
        );
        routes.extend(ladder.routes);
        routes.push(Route::html(
            "/live/browser-iframe/player.html",
            "<html><body><video controls></video><script src=\"/live/browser-iframe/player.js\"></script></body></html>"
                .to_string(),
        ));
        routes.push(Route::new(
            "/live/browser-iframe/player.js",
            "application/javascript",
            r#"(async function () {
                var p = ['/live', '/browser-iframe', '/index', '.m3u8'].join('');
                var r = await fetch(p);
                var t = await r.text();
                var urls = t.split(String.fromCharCode(10)).map(function (l) { return l.trim(); }).filter(function (l) { return l && l[0] !== '#'; });
                for (var i = 0; i < Math.min(2, urls.length); i++) {
                    try { await fetch(new URL(urls[i], location.href).toString()); } catch (e) {}
                }
            })();
"#
            .to_string()
            .into_bytes(),
        ));
        routes.push(Route::html(
            "/live/browser-iframe/page.html",
            "<html><body><iframe src=\"/live/browser-iframe/player.html\" allowfullscreen></iframe></body></html>"
                .to_string(),
        ));
    }

    // DASH manifest built at runtime (youtube şeklinin DRM'siz karşılığı).
    {
        routes.push(Route::dash(
            "/live/browser-dash/manifest.mpd",
            dash_manifest("/live/browser-dash", false),
        ));
        routes.push(Route::html(
            "/live/browser-dash/page.html",
            "<html><body><video controls></video><script src=\"/live/browser-dash/player.js\"></script></body></html>"
                .to_string(),
        ));
        routes.push(Route::new(
            "/live/browser-dash/player.js",
            "application/javascript",
            r#"(async function () {
                var p = ['/live', '/browser-dash', '/manifest', '.mpd'].join('');
                try { await fetch(p); } catch (e) {}
            })();
"#
            .to_string()
            .into_bytes(),
        ));
    }

    // detected but gated: a plain DASH manifest that a profile marks as gated.
    {
        routes.push(Route::dash(
            "/live/gated-detected/manifest.mpd",
            dash_manifest("/live/gated-detected", false),
        ));
        routes.push(Route::html(
            "/live/gated-detected/page.html",
            "<html><body><video src=\"/live/gated-detected/manifest.mpd\"></video></body></html>"
                .to_string(),
        ));
    }

    // dplayer82 shape (captured from a real episode page): the top page embeds a
    // cross-origin player iframe; the player builds a *token* manifest URL at
    // runtime and serves extension-less segments.
    {
        let ladder = hls(
            "/live/dplayer82/master",
            HlsOpts {
                segments: 4,
                expiring: true,
                ..Default::default()
            },
        );
        routes.extend(ladder.routes);
        routes.push(Route::html(
            "/live/dplayer82/page.html",
            "<html><body><iframe src=\"/live/dplayer82/player.html\" allowfullscreen></iframe></body></html>"
                .to_string(),
        ));
        routes.push(Route::html(
            "/live/dplayer82/player.html",
            "<html><body><video controls></video><script src=\"/live/dplayer82/player.js\"></script></body></html>"
                .to_string(),
        ));
        routes.push(Route::new(
            "/live/dplayer82/player.js",
            "application/javascript",
            r#"(async function () {
                var token = btoa(['token', 'part', 'x'].join(''));
                var p = ['/live', '/dplayer82', '/master', '/index.m3u8?v='].join('');
                var r = await fetch(p + token);
                var t = await r.text();
                var urls = t.split(String.fromCharCode(10)).map(function (l) { return l.trim(); }).filter(function (l) { return l && l[0] !== '#'; });
                for (var i = 0; i < Math.min(2, urls.length); i++) {
                    try { await fetch(new URL(urls[i], location.href).toString()); } catch (e) {}
                }
            })();
"#
            .to_string()
            .into_bytes(),
        ));
    }

    // deliberately undetectable: the URL only exists at runtime.
    routes.push(Route::html(
        "/live/gated/page.html",
        "<html><body><video></video><script>var u=String.fromCharCode(104,116,116,112,115)+'://'+location.host+'/vod/'+'master'+'.m3u8';</script></body></html>"
            .to_string(),
    ));

    routes
}
