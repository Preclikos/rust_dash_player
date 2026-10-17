//! ASS subtitles rendered in a Web Worker.
//!
//! The browser has no threads for the overlay's render worker, so ASS
//! frames used to render on the page's one thread, in the draw path — the
//! same thread that decodes and presents the video. Here the same renderer
//! runs in a dedicated worker instead: a second instance of this module
//! (loaded by the worker from the same `rustplayer.js`), driven through
//! [`player::ass_render::AssOffload`]. The page sends the script's text,
//! the fonts and one request per frame; the worker answers with the frame
//! as GPU masks (atlas + vertices, transferred, not copied), which the
//! overlay draws like any other.
//!
//! The worker's JavaScript is embedded below and started from a Blob URL
//! (like the audio worklet), so hosts serve no extra file. It imports the
//! module this code was loaded from; `assWorkerModuleUrl` in the player
//! options overrides that URL for bundlers that move the glue.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use js_sys::{Float32Array, Object, Reflect, Uint32Array, Uint8Array};
use player::ass::AssScript;
use player::ass_render::{self, AssOffload, AssRendered, AssRenderer, AssRequest, Frame, Layers};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

// --- worker side -------------------------------------------------------------

/// The renderer inside the worker. JS: `new AssWorker()`, then
/// `loadScript` / `addFont` / `render` as messages arrive.
#[wasm_bindgen]
pub struct AssWorker {
    renderer: AssRenderer,
    scripts: HashMap<u64, Arc<AssScript>>,
}

#[wasm_bindgen]
impl AssWorker {
    #[wasm_bindgen(constructor)]
    #[allow(clippy::new_without_default)]
    pub fn new() -> AssWorker {
        AssWorker { renderer: AssRenderer::new(), scripts: HashMap::new() }
    }

    /// Parse a script's text and shift its events, as the page did.
    #[wasm_bindgen(js_name = loadScript)]
    pub fn load_script(&mut self, id: f64, text: String, time_offset_ms: f64) -> bool {
        let Some(mut script) = player::ass::parse(&text) else {
            log::warn!("[ass worker] script {} did not parse", id);
            return false;
        };
        script.shift_events(time_offset_ms as i64);
        log::info!("[ass worker] script {}: {} events", id, script.events.len());
        self.scripts.insert(id as u64, Arc::new(script));
        true
    }

    #[wasm_bindgen(js_name = addFont)]
    pub fn add_font(&mut self, bytes: Vec<u8>) -> u32 {
        self.renderer.add_font(bytes) as u32
    }

    /// Render one frame; `{kind: "masks", left, top, width, height,
    /// atlasW, atlasH, atlas: Uint8Array, vertices: Float32Array}`,
    /// `{kind: "rgba", left, top, width, height, rgba}` or `{kind: "empty"}`.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        script: f64,
        events: Vec<u32>,
        time_ms: f64,
        width: i32,
        height: i32,
        par: f64,
        font_size_coeff: f64,
        storage_w: i32,
        storage_h: i32,
        relayout: bool,
    ) -> JsValue {
        let out = Object::new();
        let set = |k: &str, v: JsValue| {
            let _ = Reflect::set(&out, &k.into(), &v);
        };
        let Some(s) = self.scripts.get(&(script as u64)) else {
            set("kind", "empty".into());
            return out.into();
        };
        if relayout {
            self.renderer.reset_layout();
        }
        let frame = Frame {
            width,
            height,
            par,
            font_size_coeff,
            storage: (storage_w > 0 && storage_h > 0).then_some((storage_w, storage_h)),
        };
        let active: Vec<usize> = events.iter().map(|&e| e as usize).collect();
        let imgs = self.renderer.render(s, &active, time_ms as i64, frame);
        if let Some(l) = ass_render::layers(&imgs, width, height) {
            set("kind", "masks".into());
            for (k, v) in [("left", l.left), ("top", l.top)] {
                set(k, v.into());
            }
            for (k, v) in [("width", l.width), ("height", l.height), ("atlasW", l.atlas_w), ("atlasH", l.atlas_h)] {
                set(k, v.into());
            }
            set("atlas", Uint8Array::from(&l.atlas[..]).into());
            set("vertices", Float32Array::from(&l.vertices[..]).into());
        } else if let Some(c) = ass_render::composite(&imgs, width, height) {
            set("kind", "rgba".into());
            for (k, v) in [("left", c.left), ("top", c.top)] {
                set(k, v.into());
            }
            for (k, v) in [("width", c.width), ("height", c.height)] {
                set(k, v.into());
            }
            set("rgba", Uint8Array::from(&c.rgba[..]).into());
        } else {
            set("kind", "empty".into());
        }
        out.into()
    }
}

// --- page side -----------------------------------------------------------------

#[wasm_bindgen(inline_js = "export function ass_glue_url() { return new URL('../../rustplayer.js', import.meta.url).href; }")]
extern "C" {
    /// `rustplayer.js` next to this module's snippets (wasm-pack layout:
    /// `snippets/<crate>/inline0.js`).
    fn ass_glue_url() -> String;
}

/// The worker: queue messages until the module is up, then serve them.
const WORKER_JS: &str = r#"
let worker = null;
const queue = [];
function handle(m) {
  switch (m.type) {
    case 'script': worker.loadScript(m.id, m.text, m.offset); break;
    case 'font': worker.addFont(m.bytes); break;
    case 'render': {
      let out;
      try {
        out = worker.render(m.script, m.events, m.time, m.w, m.h, m.par, m.coeff, m.sw, m.sh, m.relayout);
      } catch (e) {
        out = { kind: 'empty', error: String(e) };
      }
      const transfer = [];
      if (out.atlas) transfer.push(out.atlas.buffer, out.vertices.buffer);
      if (out.rgba) transfer.push(out.rgba.buffer);
      self.postMessage({ id: m.id, out }, transfer);
      break;
    }
  }
}
self.onmessage = (e) => { if (worker) handle(e.data); else queue.push(e.data); };
import(GLUE_URL).then(async (m) => {
  await m.default();
  worker = new m.AssWorker();
  for (const d of queue) handle(d);
  queue.length = 0;
  self.postMessage({ ready: true });
}).catch((e) => self.postMessage({ failed: String(e) }));
"#;

type Done = Box<dyn FnOnce(Option<AssRendered>) + Send>;

/// [`AssOffload`] over the worker. Lives on the page's thread only.
pub struct WebAssOffload {
    worker: web_sys::Worker,
    pending: Rc<RefCell<HashMap<u32, Done>>>,
    next: Cell<u32>,
    failed: Rc<Cell<bool>>,
    _on_message: Closure<dyn FnMut(web_sys::MessageEvent)>,
    _on_error: Closure<dyn FnMut(web_sys::ErrorEvent)>,
}

// SAFETY: wasm32 here is single-threaded; the overlay calls this from the
// page's thread only (the same thread that created it).
unsafe impl Send for WebAssOffload {}
unsafe impl Sync for WebAssOffload {}

impl WebAssOffload {
    /// Start the worker importing `glue_url` (`None` = the module this code
    /// came from). `None` back when the browser refuses a module worker.
    pub fn start(glue_url: Option<String>) -> Option<WebAssOffload> {
        let glue = glue_url.unwrap_or_else(ass_glue_url);
        let src = format!("const GLUE_URL = {};\n{}", js_sys::JSON::stringify(&glue.into()).ok()?.as_string()?, WORKER_JS);
        let parts = js_sys::Array::of1(&src.into());
        let opts = web_sys::BlobPropertyBag::new();
        opts.set_type("text/javascript");
        let blob = web_sys::Blob::new_with_str_sequence_and_options(&parts, &opts).ok()?;
        let url = web_sys::Url::create_object_url_with_blob(&blob).ok()?;
        let wopts = web_sys::WorkerOptions::new();
        wopts.set_type(web_sys::WorkerType::Module);
        wopts.set_name("rustplayer-ass");
        let worker = match web_sys::Worker::new_with_options(&url, &wopts) {
            Ok(w) => w,
            Err(e) => {
                log::warn!("[ass worker] not available ({:?}); ASS renders on the page's thread", e);
                return None;
            }
        };
        let pending: Rc<RefCell<HashMap<u32, Done>>> = Rc::new(RefCell::new(HashMap::new()));
        let failed = Rc::new(Cell::new(false));
        let fail_all = {
            let (pending, failed) = (Rc::clone(&pending), Rc::clone(&failed));
            move |why: String| {
                if !failed.replace(true) {
                    log::warn!("[ass worker] failed: {}; ASS renders on the page's thread", why);
                }
                let waiting: Vec<Done> = pending.borrow_mut().drain().map(|(_, d)| d).collect();
                for done in waiting {
                    done(None);
                }
            }
        };
        let on_message = {
            let pending = Rc::clone(&pending);
            let fail_all = fail_all.clone();
            Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
                let data = e.data();
                if let Some(why) = get(&data, "failed").and_then(|v| v.as_string()) {
                    fail_all(why);
                    return;
                }
                if get(&data, "ready").is_some() {
                    log::info!("[ass worker] ready: ASS frames render off the page's thread");
                    return;
                }
                let Some(id) = get(&data, "id").and_then(|v| v.as_f64()) else { return };
                let Some(done) = pending.borrow_mut().remove(&(id as u32)) else { return };
                done(get(&data, "out").and_then(|o| rendered(&o)));
            })
        };
        let on_error = Closure::<dyn FnMut(web_sys::ErrorEvent)>::new(move |e: web_sys::ErrorEvent| {
            fail_all(e.message());
        });
        worker.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        worker.set_onerror(Some(on_error.as_ref().unchecked_ref()));
        Some(WebAssOffload { worker, pending, next: Cell::new(1), failed, _on_message: on_message, _on_error: on_error })
    }

    fn post(&self, msg: &Object, transfer: Option<&js_sys::Array>) {
        let r = match transfer {
            Some(t) => self.worker.post_message_with_transfer(msg, t),
            None => self.worker.post_message(msg),
        };
        if let Err(e) = r {
            log::warn!("[ass worker] post failed: {:?}", e);
        }
    }
}

impl AssOffload for WebAssOffload {
    fn available(&self) -> bool {
        !self.failed.get()
    }

    fn load_script(&self, id: u64, source: &player::ass::AssSource) {
        let m = Object::new();
        let _ = Reflect::set(&m, &"type".into(), &"script".into());
        let _ = Reflect::set(&m, &"id".into(), &(id as f64).into());
        let _ = Reflect::set(&m, &"text".into(), &JsValue::from_str(&source.text));
        let _ = Reflect::set(&m, &"offset".into(), &(source.time_offset_ms as f64).into());
        self.post(&m, None);
    }

    fn add_font(&self, bytes: Arc<[u8]>) {
        let m = Object::new();
        let arr = Uint8Array::from(&bytes[..]);
        let _ = Reflect::set(&m, &"type".into(), &"font".into());
        let _ = Reflect::set(&m, &"bytes".into(), &arr);
        self.post(&m, Some(&js_sys::Array::of1(&arr.buffer())));
    }

    fn render(&self, request: AssRequest, done: Done) {
        if self.failed.get() {
            done(None);
            return;
        }
        let id = self.next.get();
        self.next.set(id.wrapping_add(1));
        self.pending.borrow_mut().insert(id, done);
        let m = Object::new();
        let f = &request.frame;
        let (sw, sh) = f.storage.unwrap_or((0, 0));
        let set = |k: &str, v: JsValue| {
            let _ = Reflect::set(&m, &k.into(), &v);
        };
        set("type", "render".into());
        set("id", id.into());
        set("script", (request.script as f64).into());
        set("events", Uint32Array::from(&request.events[..]).into());
        set("time", (request.time_ms as f64).into());
        set("w", f.width.into());
        set("h", f.height.into());
        set("par", f.par.into());
        set("coeff", f.font_size_coeff.into());
        set("sw", sw.into());
        set("sh", sh.into());
        set("relayout", request.relayout.into());
        self.post(&m, None);
    }
}

fn get(obj: &JsValue, key: &str) -> Option<JsValue> {
    Reflect::get(obj, &JsValue::from_str(key)).ok().filter(|v| !v.is_undefined() && !v.is_null())
}

/// The worker's answer as a frame.
fn rendered(out: &JsValue) -> Option<AssRendered> {
    let num = |k: &str| get(out, k).and_then(|v| v.as_f64()).unwrap_or(0.0);
    match get(out, "kind")?.as_string()?.as_str() {
        "masks" => Some(AssRendered::Masks(Layers {
            left: num("left") as i32,
            top: num("top") as i32,
            width: num("width") as u32,
            height: num("height") as u32,
            atlas: get(out, "atlas")?.dyn_into::<Uint8Array>().ok()?.to_vec(),
            atlas_w: num("atlasW") as u32,
            atlas_h: num("atlasH") as u32,
            vertices: get(out, "vertices")?.dyn_into::<Float32Array>().ok()?.to_vec(),
        })),
        "rgba" => Some(AssRendered::Rgba(ass_render::Composite {
            left: num("left") as i32,
            top: num("top") as i32,
            width: num("width") as u32,
            height: num("height") as u32,
            rgba: get(out, "rgba")?.dyn_into::<Uint8Array>().ok()?.to_vec(),
        })),
        _ => {
            if let Some(err) = get(out, "error").and_then(|v| v.as_string()) {
                log::warn!("[ass worker] render failed: {}", err);
            }
            None
        }
    }
}
