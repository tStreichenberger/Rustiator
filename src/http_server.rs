use actix::Addr;
use actix_web::Error;
use actix_web_actors::ws;
use std::sync::mpsc;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
};
use tokio::task::JoinSet;

use actix_cors::Cors;
use actix_files as fs;
use actix_web::{
    get, http, post, web, web::Json, App, HttpRequest, HttpResponse, HttpServer, Responder,
};

use dashmap::DashMap;
use handlebars::Handlebars;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::websocket::MyWebSocket;
use crate::{chess_engine::engine_vs_engine, chess_game::ChessGame};

pub type GameMap = DashMap<Uuid, Arc<RwLock<ChessGame>>>;
pub type Connection = Addr<MyWebSocket>;
pub type SharedState = Arc<RwLock<Vec<Connection>>>;

#[derive(Deserialize, Debug)]
struct NewGameArgs {
    mode: String,
}

#[get("/ping")]
async fn ping() -> impl Responder {
    HttpResponse::Ok().body("pong".to_string())
}

#[post("/new_game")]
async fn new_game(
    app_data: web::Data<GameMap>,
    active_processes: web::Data<Arc<Mutex<HashMap<Uuid, JoinSet<()>>>>>,
    connections: web::Data<DashMap<Uuid, SharedState>>,
    req_body: Json<NewGameArgs>,
) -> impl Responder {

    println!("recieved request!");
    // Generate a new UUID
    let new_game_id = Uuid::new_v4();

    match req_body.mode.as_str() {
        "playerVsBot" => {
            let game = Arc::new(RwLock::new(ChessGame::new()));
            app_data.insert(new_game_id, game);
        }
        "botVsBot" => {
            let game = Arc::new(RwLock::new(ChessGame::new()));
            let engine1 = Arc::new(crate::chess_engine::RandomEngine::new());
            let engine2 = Arc::new(crate::chess_engine::RandomEngine::new());

            let game_clone = game.clone();
            let engine1_clone = engine1.clone();
            let engine2_clone = engine2.clone();

            let (tx, rx) = mpsc::channel();

            let mut game_join_set = JoinSet::new();

            game_join_set.spawn_blocking(move || {
                engine_vs_engine(game_clone, engine1_clone, engine2_clone, tx);
            });

            let new_game_connections: SharedState = Arc::new(RwLock::new(Vec::new()));
            connections.insert(new_game_id, new_game_connections.clone());
            game_join_set.spawn_blocking(move || {
                loop {
                    let result = match rx.recv() {
                        Ok(message) => message,
                        Err(e) => {
                            eprintln!("Game over, terminating the thread: {:?}", e);
                            break; // Exit the loop and end the thread
                        }
                    };
            
                    let connections_clone = new_game_connections.read().unwrap();
                    for conn in connections_clone.iter() {
                        conn.do_send(result.clone()); 
                    }
                }
            });

            // not doing anything with set right now, but save in case in the future
            // we want to do some graceful shutdown logic
            let mut active_tasks = active_processes.lock().unwrap();
            active_tasks.insert(new_game_id, game_join_set);

            println!("inserted game {} into active tasks", new_game_id);
            app_data.insert(new_game_id, game);
        }
        _ => {
            return HttpResponse::BadRequest().body("Invalid game mode");
        }
    }

    // Return the new game ID to the client
    println!("reached return ");
    HttpResponse::Ok().json(serde_json::json!({ "game_id": new_game_id.to_string() }))
}

#[get("/spectate/{uuid}")]
async fn spectate_game(
    app_data: web::Data<GameMap>,
    hb: web::Data<Handlebars<'_>>,
    info: web::Path<Uuid>,
) -> impl Responder {
    let game_uuid = info.into_inner();

    // Fetch the game data
    let game_data = match app_data.get(&game_uuid) {
        Some(game) => game,
        None => return HttpResponse::NotFound().body("Game not found"),
    };

    let gd_lock = match game_data.read() {
        Ok(gd) => gd,
        Err(_) => return HttpResponse::InternalServerError().body("Failed to lock game data"),
    };

    let position = gd_lock.fen();
    let css_content =
        std::fs::read_to_string("./client/css/chessboard-1.0.0.min.css").unwrap();
    let js_content = std::fs::read_to_string("./client/js/chessboard-1.0.0.js").unwrap();

    // Create data to fill the template
    let data = json!({
        "game_id": game_uuid.to_string(),
        "position": position,
        "style": css_content,
        "board_js":js_content
    });

    // Render the template with the data
    let body = hb.render("spectate_template", &data).unwrap_or_else(|err| {
        println!("Template rendering error: {}", err);
        "Template rendering error".to_string()
    });

    HttpResponse::Ok().content_type("text/html").body(body)
}

pub async fn ws_index(
    req: HttpRequest,
    stream: web::Payload,
    uuid: web::Path<Uuid>, // Extract UUID from the path
    connections: web::Data<DashMap<Uuid, SharedState>>
) -> Result<HttpResponse, Error> {
    println!("New Connection to Game: {}", &uuid);
    match connections.get(&uuid) {
        Some(game_conns) => {
            let game_conns: SharedState = game_conns.clone();
            let ws = MyWebSocket::new(game_conns);
            ws::start(ws, &req, stream)
        }
        None => {
            let err_msg = format!("Room {} not found", &uuid);
            let err = std::io::Error::new(std::io::ErrorKind::NotFound, err_msg);
            return Err(err.into());
        }
    }
}
pub async fn start_server(hostname: String, port: u16) -> std::io::Result<()> {
    // Init an empty hashmap to store all the ongoing processes
    let active = Arc::new(Mutex::new(
        HashMap::<Uuid, JoinSet<()>>::new(),
    ));
    let active_tasks = web::Data::new(active);

    // Initialize an empty hashmap which maps UUID to ChessGame 
    let games: GameMap = DashMap::new();
    let games_data = web::Data::new(games);

    let mut handlebars = Handlebars::new();
    handlebars
        .register_template_file("spectate_template", "./client/spectate.html")
        .unwrap(); // lol fix
    let handlebars_ref = web::Data::new(handlebars);

    // Active Spectator connections 
    let connections: DashMap<Uuid, SharedState> = DashMap::new();
    let connections_data = web::Data::new(connections);

    println!("Starting server on {}:{}", hostname, port);
    let allowed_origin = format!("http://{}:{}", &hostname, &port);
    HttpServer::new(move || {
        let cors = Cors::default()
            .allowed_origin(&allowed_origin)
            .allowed_methods(vec!["GET", "POST", "PUT", "DELETE", "OPTIONS"])
            .allowed_headers(vec![http::header::AUTHORIZATION, http::header::ACCEPT])
            .allowed_header(http::header::CONTENT_TYPE)
            .max_age(3600);

        // because we are doing a wildcard match with render game, the order of the
        // routes actually does matter here
        App::new()
            .wrap(cors)
            .app_data(games_data.clone()) // Add the shared state to the app
            .app_data(handlebars_ref.clone())
            .app_data(active_tasks.clone())
            .app_data(connections_data.clone())
            .route("/ws/{uuid}", web::get().to(ws_index))
            .service(spectate_game)
            .service(new_game)
            .service(fs::Files::new("/", "./client/").index_file("index.html"))
            .service(fs::Files::new("/img", "./client/img"))

    })
    .workers(4) // Set the number of worker threads
    .bind(("0.0.0.0", port))?
    .run()
    .await
}
