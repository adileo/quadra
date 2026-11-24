// The web studio built into the binary: `quadra serve` and `quadra dev` serve it without any
// other file. The studio is not in this repository (it has its own license): the official
// binaries embed it, a build from these sources embeds the folder `QUADRA_WEB_DIST` names if
// it is set, and otherwise a page that points to the official binaries.
use std::path::{Path, PathBuf};

const NO_STUDIO: &str = r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Quadra</title>
<style>body{font:16px/1.55 system-ui,sans-serif;margin:0;display:grid;place-items:center;min-height:100vh;background:#f7f6fb;color:#16131f}main{max-width:32rem;padding:2rem}h1{font-size:1.3rem;margin:0 0 .6rem}p{margin:.4rem 0;color:#5b5670}code{font-family:ui-monospace,Menlo,monospace}a{color:#6d28d9}@media(prefers-color-scheme:dark){body{background:#0b0d14;color:#f2f0f8}p{color:#a19cb5}a{color:#a78bfa}}</style></head>
<body><main><h1>The API is running</h1><p>This build of Quadra comes from the sources and has no studio (the web interface): the API answers at <code>/v1</code>.</p>
<p>The official binaries include the studio: <a href="https://quadra.run/guide/installation">quadra.run/guide/installation</a>.</p></main></body></html>"#;

fn main() {
    println!("cargo:rerun-if-env-changed=QUADRA_WEB_DIST");
    let out = Path::new(&std::env::var("OUT_DIR").unwrap()).join("webapp.rs");
    let dist = std::env::var_os("QUADRA_WEB_DIST").map(PathBuf::from);
    let Some(dist) = dist.filter(|d| d.join("index.html").is_file()) else {
        if let Some(d) = std::env::var_os("QUADRA_WEB_DIST") {
            println!("cargo:warning=QUADRA_WEB_DIST={} has no index.html: building without the studio", Path::new(&d).display());
        }
        std::fs::write(&out, format!("pub static FILES: &[(&str, &[u8])] = &[(\"index.html\", {:?}.as_bytes())];\n", NO_STUDIO)).unwrap();
        return;
    };
    println!("cargo:rerun-if-changed={}", dist.display());
    let mut files: Vec<PathBuf> = Vec::new();
    let mut todo = vec![dist.clone()];
    while let Some(dir) = todo.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                todo.push(p);
            } else {
                files.push(p);
            }
        }
    }
    files.sort();
    let mut s = String::from("pub static FILES: &[(&str, &[u8])] = &[\n");
    for f in &files {
        let rel = f.strip_prefix(&dist).unwrap().to_string_lossy().replace('\\', "/");
        s.push_str(&format!("    ({rel:?}, include_bytes!({:?})),\n", f.canonicalize().unwrap()));
    }
    s.push_str("];\n");
    std::fs::write(out, s).unwrap();
}
