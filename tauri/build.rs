//! Extracts the dashboard and tray-popup HTML from `desktop/web.py` (the single source of
//! truth shared with the Python app) into `ui/`, with a small shim that maps the pages'
//! `fetch("/api/...")` and `window.pywebview.api` calls onto Tauri commands.
use std::fs;

const SHIM: &str = r#"<script>
(()=>{const inv=(c,a)=>window.__TAURI__.core.invoke(c,a);
const json=v=>new Response(JSON.stringify(v),{status:200,headers:{"content-type":"application/json"}});
const of=window.fetch.bind(window);
window.fetch=(u,o)=>{const s=String(u); if(!s.startsWith("/api/")) return of(u,o);
  const url=new URL(s,"http://local"), q=url.searchParams; let p;
  if(url.pathname==="/api/local/summary") p=inv("summary",{days:Number(q.get("days")||30),tool:q.get("tool")||null,project:q.get("project")||null});
  else if(url.pathname==="/api/local/utilization") p=inv("utilization");
  else if(url.pathname==="/api/local/status") p=inv("status");
  else if(url.pathname==="/api/theme") p=inv("theme",{theme:q.get("theme")});
  else return Promise.resolve(new Response("not found",{status:404}));
  return p.then(json,e=>new Response(JSON.stringify({error:String(e)}),{status:500}));};
window.pywebview={api:{resize:(w,h)=>inv("popup_resize",{w,h}),startup_enabled:()=>inv("startup_enabled"),
  startup_label:()=>inv("startup_label"),toggle_startup:()=>inv("toggle_startup"),
  show_dashboard:()=>inv("show_dashboard"),quit_app:()=>inv("quit_app"),dismiss:()=>inv("dismiss")}};
document.addEventListener("click",e=>{const a=e.target.closest&&e.target.closest('a[target="_blank"]');
  if(a&&/^https:\/\//.test(a.href)){e.preventDefault();inv("open_url",{url:a.href});}},true);
document.addEventListener("click",e=>{const a=e.target.closest&&e.target.closest("a[download]");
  if(!a||!a.href.startsWith("blob:"))return; e.preventDefault();  // WebView downloads are off: save via Rust
  fetch(a.href).then(r=>r.text()).then(content=>inv("save_export",{name:a.download,content}));},true);
})();
</script>"#;

fn extract(src: &str, name: &str) -> String {
    let start = src.find(&format!("{name} = r\"\"\"")).expect("html constant") + name.len() + 7;
    let end = start + src[start..].find("\"\"\"").expect("closing quotes");
    let html = &src[start..end];
    let at = html.find("<head>").expect("<head>") + "<head>".len();
    format!("{}\n{}{}", &html[..at], SHIM, &html[at..])
}

fn main() {
    let web = "../desktop/web.py";
    println!("cargo:rerun-if-changed={web}");
    let src = fs::read_to_string(web).expect("read desktop/web.py");
    fs::create_dir_all("ui").unwrap();
    fs::write("ui/index.html", extract(&src, "INDEX_HTML")).unwrap();
    fs::write("ui/tray.html", extract(&src, "TRAY_HTML")).unwrap();
    tauri_build::build()
}
