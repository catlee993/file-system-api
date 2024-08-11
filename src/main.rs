use gstreamer::prelude::*;
use actix::prelude::*;
use actix_files::{NamedFile};
use actix_web::{web, App, HttpServer, Responder, HttpResponse, HttpRequest, Error};
use actix_web_actors::ws;
use gstreamer as gst;
use serde::Serialize;
use std::{fs};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::sync::mpsc::Receiver;
use actix_cors::Cors;
use actix_web::web::Bytes;
use gstreamer::glib::GString;
use gstreamer::prelude::{ElementExt, GstObjectExt};
use gstreamer_app as gst_app;

#[derive(Serialize)]
struct VideoList {
    videos: Vec<String>,
}

struct AppState {
    active_connections: Arc<AtomicUsize>,
    pipeline: Arc<Mutex<Option<gst::Pipeline>>>,
}

async fn list_videos() -> impl Responder {
    let base_path = PathBuf::from("./videos");
    let mut videos = vec![];

    match fs::read_dir(&base_path) {
        Ok(entries) => {
            for entry in entries {
                match entry {
                    Ok(entry) => {
                        if entry.path().is_file() {
                            videos.push(entry.file_name().into_string().unwrap());
                        } else {
                            println!("Skipping directory: {:?}", entry.path());
                        }
                    }
                    Err(e) => println!("Error reading entry: {:?}", e),
                }
            }
        }
        Err(e) => println!("Failed to read directory: {:?}", e),
    }

    videos.sort();

    let response = VideoList { videos };
    HttpResponse::Ok().json(response)
}

async fn stream_video(id: web::Path<String>, req: HttpRequest) -> impl Responder {
    let path = PathBuf::from(format!("./videos/{}", id));
    if path.exists() && path.is_file() {
        NamedFile::open(path).unwrap().into_response(&req)
    } else {
        HttpResponse::NotFound().body("Video not found")
    }
}

struct VideoStream {
    frame_receiver: Option<Receiver<Vec<u8>>>,
    app_state: web::Data<AppState>,
}

impl Actor for VideoStream {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        let active_connections = self.app_state.active_connections.clone();
        active_connections.fetch_add(1, Ordering::SeqCst);
        println!("WebSocket connected: {}", active_connections.load(Ordering::SeqCst));

        if self.frame_receiver.is_none() {
            let (tx, rx) = mpsc::channel();
            self.frame_receiver = Some(rx);
            start_streaming(self.app_state.clone(), tx, Arc::new(Mutex::new(ctx.address())));
        }
    }

    fn stopped(&mut self, _: &mut Self::Context) {
        let active_connections = self.app_state.active_connections.clone();
        if active_connections.fetch_sub(1, Ordering::SeqCst) == 1 {
            println!("Last WebSocket disconnected, stopping stream.");
            stop_streaming(self.app_state.clone());
        } else {
            println!("WebSocket disconnected: {}", active_connections.load(Ordering::SeqCst));
        }
    }
}

struct VideoStreamMessage {
    data: Bytes,
}

impl Message for VideoStreamMessage {
    type Result = ();
}

impl Handler<VideoStreamMessage> for VideoStream {
    type Result = ();

    fn handle(&mut self, msg: VideoStreamMessage, ctx: &mut Self::Context) {
        ctx.binary(msg.data);
    }
}

fn start_streaming(app_state: web::Data<AppState>, frame_tx: mpsc::Sender<Vec<u8>>, ctx: Arc<Mutex<Addr<VideoStream>>>) {
    let active_connections = app_state.active_connections.clone();
    let pipeline = gst::Pipeline::new();

    let source = gst::ElementFactory::make("rtspsrc")
        .property("location", "rtsp://127.0.0.1:8554/E1")
        .build()
        .expect("Failed to create rtspsrc element");

    let depay = gst::ElementFactory::make("rtph264depay")
        .build()
        .expect("Failed to create rtph264depay element");

    let decode = gst::ElementFactory::make("avdec_h264")
        .build()
        .expect("Failed to create avdec_h264 element");

    let convert = gst::ElementFactory::make("videoconvert")
        .build()
        .expect("Failed to create videoconvert element");

    let capsfilter = gst::ElementFactory::make("capsfilter")
        .property("caps", &gst::Caps::builder("video/x-raw").field("format", &"I420").build())
        .build()
        .expect("Failed to create capsfilter element");

    let appsink = gst::ElementFactory::make("appsink")
        .property("emit-signals", &true)
        .property("sync", &false)
        .build()
        .expect("Failed to create appsink element");

    let encoder = gst::ElementFactory::make("jpegenc")
        .build()
        .expect("Failed to create jpegenc element");

    let appsink = appsink
        .dynamic_cast::<gst_app::AppSink>()
        .expect("Failed to cast to AppSink");

    let frame_tx = Arc::new(Mutex::new(frame_tx));

    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().unwrap();
                let buffer = sample.buffer().unwrap();
                let map = buffer.map_readable().unwrap();

                let frame_data = map.as_slice().to_vec();
                let frame_tx = frame_tx.lock().unwrap();
                if let Err(e) = frame_tx.send(frame_data.clone()) {
                    println!("Failed to send frame data: {:?}", e);
                }

                let ctx = ctx.lock().unwrap();
                ctx.do_send(VideoStreamMessage { data: Bytes::copy_from_slice(&frame_data) });

                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    pipeline.add_many(&[&source, &depay, &decode, &convert, &capsfilter, &encoder, &appsink.upcast_ref::<gst::Element>()]).unwrap();
    gst::Element::link_many(&[&depay, &decode, &convert, &capsfilter, &encoder, &appsink.upcast_ref::<gst::Element>()]).unwrap();

    source.connect_pad_added(move |_, src_pad| {
        let sink_pad = depay.static_pad("sink").unwrap();

        if sink_pad.is_linked() {
            println!("Pad already linked, skipping.");
            return;
        }

        let src_caps = src_pad.current_caps().unwrap();
        let src_struct = src_caps.structure(0).unwrap();
        let src_media_type = src_struct.name();

        if src_media_type.starts_with("application/x-rtp") {
            if let Err(err) = src_pad.link(&sink_pad) {
                println!("Failed to link pads: {:?}", err);
            }
        } else {
            println!("Pad has incompatible format: {}", src_media_type);
        }
    });

    {
        let mut pipeline_guard = app_state.pipeline.lock().unwrap();
        *pipeline_guard = Some(pipeline.clone());
    }

    std::thread::spawn(move || {
        gst::init().unwrap();

        let bus = pipeline.bus().unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();

        for msg in bus.iter_timed(gst::ClockTime::NONE) {
            match msg.view() {
                gst::MessageView::Application(app_msg) => {
                    if app_msg.structure().map(|s| s.name().as_str()) == Some("stop-pipeline") {
                        break;
                    }
                }
                gst::MessageView::Eos(..) => break,
                gst::MessageView::Error(err) => {
                    eprintln!("Error from element {}: {} ({:?})", err.src().map(|s| s.path_string()).unwrap_or_else(|| GString::from(String::from("None"))), err.error(), err.debug());
                    break;
                }
                _ => (),
            }

            if active_connections.load(Ordering::SeqCst) == 0 {
                println!("No active connections. Stopping pipeline.");
                break;
            }
        }

        pipeline.set_state(gst::State::Null).unwrap();
        println!("Pipeline stopped.");
    });
}

fn stop_streaming(app_state: web::Data<AppState>) {
    if let Some(pipeline) = app_state.pipeline.lock().unwrap().as_ref() {
        println!("Sending stop signal to pipeline.");

        let structure = gst::Structure::builder("stop-pipeline").build();
        let app_msg = gst::message::Application::builder(structure).build();

        if let Some(bus) = pipeline.bus() {
            bus.post(app_msg).expect("Failed to post stop message to pipeline");
        }
    }
}

async fn ws_video_stream(
    req: HttpRequest,
    stream: web::Payload,
    app_state: web::Data<AppState>
) -> Result<HttpResponse, Error> {
    let ws = VideoStream {
        frame_receiver: None,
        app_state: app_state.clone(),
    };
    let resp = ws::start(ws, &req, stream)?;
    Ok(resp)
}

fn execute_ptz_command(direction: &str) -> HttpResponse {
    let output = Command::new("./neolink")
        .arg("ptz")
        .arg("--config=neolink.toml")
        .arg("Camera01")
        .arg("control")
        .arg("32")
        .arg(direction)
        .output();

    match output {
        Ok(output) if output.status.success() => HttpResponse::Ok().body(format!("PTZ {} command executed successfully", direction)),
        Ok(output) => HttpResponse::InternalServerError().body(format!("Failed to execute PTZ command: {:?}", output)),
        Err(err) => HttpResponse::InternalServerError().body(format!("Error executing PTZ command: {:?}", err)),
    }
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for VideoStream {
    fn handle(&mut self, msg: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        println!("Received WebSocket message: {:?}", msg);
        match msg {
            Ok(ws::Message::Ping(msg)) => ctx.pong(&msg),
            Ok(ws::Message::Pong(_)) => (),
            Ok(ws::Message::Text(_)) => (),
            Ok(ws::Message::Binary(_)) => (),
            Ok(ws::Message::Close(reason)) => {
                ctx.close(reason);
                ctx.stop();
            }
            _ => ctx.stop(),
        }
    }
}

fn execute_zoom_command(direction: &str, parameter: &str) -> HttpResponse {
    let output = Command::new("./neolink")
        .arg("ptz")
        .arg("--config=neolink.toml")
        .arg("Camera01")
        .arg(direction)
        .arg(parameter)
        .output();

    match output {
        Ok(output) if output.status.success() => HttpResponse::Ok().body(format!("Zoom {} command executed successfully", direction)),
        Ok(output) => HttpResponse::InternalServerError().body(format!("Failed to execute Zoom command: {:?}", output)),
        Err(err) => HttpResponse::InternalServerError().body(format!("Error executing Zoom command: {:?}", err)),
    }
}

// using neolink to control PTZ (seems to be camera model dependent)
async fn ptz_up() -> impl Responder {
    execute_ptz_command("up")
}

async fn ptz_down() -> impl Responder {
    execute_ptz_command("down")
}

async fn ptz_left() -> impl Responder {
    execute_ptz_command("left")
}

async fn ptz_right() -> impl Responder {
    execute_ptz_command("right")
}

async fn zoom_in() -> impl Responder {
    execute_zoom_command("zoom", "2.5")
}

async fn zoom_out() -> impl Responder {
    execute_zoom_command("zoom", "1.0")
}

#[actix_rt::main]
async fn main() -> std::io::Result<()> {
    gst::init().unwrap();
    let active_connections = Arc::new(AtomicUsize::new(0));
    let pipeline = Arc::new(Mutex::new(None));

    let app_state = web::Data::new(AppState {
        active_connections: active_connections.clone(),
        pipeline: pipeline.clone(),
    });

    HttpServer::new(move || {
        App::new()
            .app_data(app_state.clone())
            .wrap(
                Cors::default()
                    .allow_any_origin()
                    .allow_any_method()
                    .allow_any_header()
                    .max_age(3600)
            )
            .route("/videos", web::get().to(list_videos))
            .route("/videos/{id}", web::get().to(stream_video))
            .route("/ptz/up", web::get().to(ptz_up))
            .route("/ptz/down", web::get().to(ptz_down))
            .route("/ptz/left", web::get().to(ptz_left))
            .route("/ptz/right", web::get().to(ptz_right))
            .route("/ptz/in", web::get().to(zoom_in))
            .route("/ptz/out", web::get().to(zoom_out))
            .route("/ws/", web::get().to(ws_video_stream))
    })
        .bind("0.0.0.0:8080")?
        .run()
        .await
}
