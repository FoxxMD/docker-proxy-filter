use ::http::StatusCode;
use futures_util::TryStreamExt;
use ntex::{
    http::{self, HttpMessage},
    web::{self},
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing::*;

use crate::config::AppConfig;
use crate::docker::{self, types::*};
use crate::utils;

#[derive(Clone)]
pub struct AppStateWithContainerMap {
    pub container_map: Arc<Mutex<HashMap<String, Option<bool>>>>, // <- Mutex is necessary to mutate safely across threads
}

macro_rules! is {
    ($cond:expr; $if:expr; $else:expr) => {
        if $cond { $if } else { $else }
    };
}

pub async fn forward(
    req: web::HttpRequest,
    body: ntex::util::Bytes,
    client: web::types::State<http::Client>,
    forward_url: web::types::State<url::Url>,
    app_config: web::types::State<AppConfig>,
    data: web::types::State<AppStateWithContainerMap>,
) -> Result<web::HttpResponse, web::Error> {
    let mut new_url = forward_url.get_ref().clone();
    new_url.set_path(req.uri().path());
    new_url.set_query(req.uri().query());
    let forwarded_req = client.request_from(new_url.as_str(), req.head());
    let mut res = forwarded_req
        .send_body(body)
        .await
        .map_err(web::Error::from)?;

    let mut client_resp = web::HttpResponse::build(res.status());

    // respond without body for /_ping HEAD request
    if req.uri().path() == "/_ping" && req.method() == http::Method::HEAD {
        for (key, value) in res.headers().iter() {
            client_resp.header(key.clone(), value.clone());
        }
        return Ok(client_resp.finish());
    }

    client_resp.content_type(res.content_type());

    if res.status() == 200 {
        // if route is to list containers we want to return a filtered list
        if new_url.path().contains("containers/json") {
            let _list_span = span!(Level::DEBUG, "Container List").entered();

            let container_res = &res
                .json::<Vec<ContainerSummary>>()
                // 2mb in bytes
                .limit(2097152)
                .await;

            let containers = match container_res {
                Ok(list_res) => list_res,
                Err(e) => {
                    panic!("{e}");
                }
            };

            // filter all containers to only those that have values from CONTAINER_NAMES includes in their names
            let filtered_containers = containers
                .into_iter()
                .filter(|&con| {
                    let _list_span = span!(
                        Level::DEBUG,
                        "Container",
                        id = utils::short_id(con.id.as_ref().unwrap())
                    )
                    .entered();
                    docker::container_summary_match(
                        con,
                        &app_config.get_ref().container_names,
                        &app_config.get_ref().container_labels,
                    )
                })
                .collect::<Vec<&ContainerSummary>>();

            let fresp = web::HttpResponse::build(res.status()).json(&filtered_containers);
            debug!(
                "{} of {} containers valid",
                filtered_containers.len(),
                containers.len()
            );
            Ok(fresp)
        } else {
            // only deal with routes that are for containers like /containers/1234/{some_resource}
            match docker::match_container_get(new_url.path()) {
                // the regex pulls the container id from the route with a named capture group, avaiable as m.id
                Some(m) => {
                    let short_cid = utils::short_id(&m.id); //format!("{start}...{end}", start = &m.id[..6], end = &m.id[&m.id.len() - 6..]);
                    let _con_span = span!(Level::DEBUG, "Container", id = short_cid).entered();
                    debug!("Matched container route with Id {}", &m.id);
                    // we keep a stateful hashmap of all requested container ids and their names
                    // see AppStateWithContainerMap
                    let mut cm = data.container_map.lock().unwrap();

                    // if the map does not already include the container id-name then we try to get it with our own request to docker api
                    // or if we encountered an error last time then try again
                    if !cm.contains_key(&m.id) || cm.get(&m.id).unwrap().is_none() {
                        debug!("Requested Id not in map, trying to inspect...");
                        let info_res =
                            docker::get_container_info(&client, &forward_url, &m.id).await;
                        match info_res {
                            Ok((name, labels)) => {
                                let is_container_match = docker::match_labels_or_names(
                                    &app_config.get_ref().container_names,
                                    &app_config.get_ref().container_labels,
                                    &Vec::from([name.clone()]),
                                    &labels,
                                );
                                debug!(
                                    "Recording container '{}' {} valid",
                                    name,
                                    is!(is_container_match; "as";"as not")
                                );
                                cm.insert(m.id.clone(), Some(is_container_match));
                            }
                            Err(e) => {
                                warn!("Could not inspect: {e}");
                                if e.to_string().contains("No such container") {
                                    cm.insert(m.id.clone(), Some(false));
                                } else {
                                    // if the error was not a 404 then allow trying again later
                                    cm.insert(m.id.clone(), None);
                                }
                            }
                        }
                    }

                    // then we try to get the name from the stateful hashmap
                    let allowed = cm.get(&m.id).unwrap();

                    match allowed {
                        Some(n) => {
                            // only return if a response if requested container has a name  that includes values from CONTAINER_NAMES
                            if *n {
                                debug!("Matched container filters");
                                // if the route resource is specifically the Container Inspect API
                                // then we may need to scrub Envs if SCRUB_ENVS=true
                                if m.resource == "json" {
                                    client_resp.content_type("application/json");

                                    if app_config.get_ref().scrub_envs {
                                        let mut container =
                                            res.json::<ContainerInspect>().await.unwrap();
                                        container.config.as_mut().unwrap().env = Some(Vec::new());
                                        Ok(client_resp.json(&container))
                                    } else {
                                        Ok(client_resp.streaming(res.into_stream()))
                                    }
                                } else {
                                    client_resp.content_type(res.content_type());
                                    Ok(client_resp.streaming(res.into_stream()))
                                }
                            } else {
                                debug!("Does not match container filters, 404ing...");
                                client_resp.status(StatusCode::NOT_FOUND);
                                Ok(client_resp.json(&DockerErrorMessage {
                                    message: format!("No such container: {}", &m.id),
                                }))
                            }
                        }
                        None => {
                            debug!(
                                "Does not exist or Docker API previously returned an error, 404ing..."
                            );
                            client_resp.status(StatusCode::NOT_FOUND);
                            Ok(client_resp.json(&DockerErrorMessage {
                                message: format!("No such container: {}", &m.id),
                            }))
                        }
                    }
                }
                // if we don't match container route then proxy response through, unmodified
                None => {
                    let stream = res.into_stream();
                    Ok(client_resp.streaming(stream))
                }
            }
        }
    } else {
        let stream = res.into_stream();
        Ok(client_resp.streaming(stream))
    }
}
