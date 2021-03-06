// Copyright (c) 2016-2017 Chef Software Inc. and/or applicable contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! A collection of handlers for the HTTP server's router

use std::env;

use base64;
use bodyparser;
use bldr_core::helpers::transition_visibility;
use depot::server::check_origin_access;
use github_api_client::HubError;
use hab_core::package::{Identifiable, Plan};
use hab_core::event::*;
use http_gateway::http::controller::*;
use http_gateway::http::helpers::{self, validate_params};
use iron::status;
use params::{Params, Value, FromValue};
use persistent;
use protocol::jobsrv::{Job, JobGet, JobLogGet, JobLog, JobState, ProjectJobsGet,
                       ProjectJobsGetResponse};
use protocol::scheduler::{ReverseDependenciesGet, ReverseDependencies};
use protocol::originsrv::*;
use protocol::sessionsrv;
use router::Router;
use serde_json;
use typemap;

use github;
use headers::*;
use types::*;

// A default name for per-project integrations. Currently, there
// can only be one.
const DEFAULT_PROJECT_INTEGRATION: &'static str = "default";

define_event_log!();

pub fn github_authenticate(req: &mut Request) -> IronResult<Response> {
    let code = {
        let params = req.extensions.get::<Router>().unwrap();
        params.find("code").unwrap().to_string()
    };

    let github = req.get::<persistent::Read<GitHubCli>>().unwrap();

    if env::var_os("HAB_FUNC_TEST").is_some() {
        let session = session_create(&github, &code)?;

        log_event!(
            req,
            Event::GithubAuthenticate {
                user: session.get_name().to_string(),
                account: session.get_id().to_string(),
            }
        );

        return Ok(render_json(status::Ok, &session));
    }

    match github.authenticate(&code) {
        Ok(token) => {
            let session = session_create(&github, &token)?;

            log_event!(
                req,
                Event::GithubAuthenticate {
                    user: session.get_name().to_string(),
                    account: session.get_id().to_string(),
                }
            );

            Ok(render_json(status::Ok, &session))
        }
        Err(HubError::Auth(e)) => {
            let err = NetError::new(ErrCode::ACCESS_DENIED, e.error);
            Ok(render_net_error(&err))
        }
        Err(HubError::Serialization(e)) => {
            warn!("bad reply from GitHub, {}", e);
            let err = NetError::new(ErrCode::BAD_REMOTE_REPLY, "rg:auth:1");
            Ok(render_net_error(&err))
        }
        Err(e) => {
            warn!("unhandled github authentication error, {:?}", e);
            let err = NetError::new(ErrCode::BUG, "rg:auth:2");
            Ok(render_net_error(&err))
        }
    }
}

pub fn job_group_promote(req: &mut Request) -> IronResult<Response> {
    // JB TODO: eliminate the need to clone the params and session - HI SALIM =)
    let params = req.extensions.get::<Router>().unwrap().clone();
    let session = req.extensions.get::<Authenticated>().unwrap().clone();
    let session_id = session.get_id();

    let group_id = match params.find("id") {
        Some(id) => {
            match id.parse::<u64>() {
                Ok(g) => g,
                Err(e) => {
                    debug!("Error finding group. e = {:?}", e);
                    return Ok(Response::with(status::BadRequest));
                }
            }
        }
        None => return Ok(Response::with(status::BadRequest)),
    };

    let channel = match params.find("channel") {
        Some(c) => c.to_string(),
        None => return Ok(Response::with(status::BadRequest)),
    };

    match helpers::promote_job_group_to_channel(req, group_id, &channel, Some(session_id)) {
        Ok(resp) => Ok(render_json(status::Ok, &resp)),
        Err(err) => Ok(render_net_error(&err)),
    }
}

pub fn project_privacy_toggle(req: &mut Request) -> IronResult<Response> {
    let session = req.extensions.get::<Authenticated>().unwrap().clone();
    let params = req.extensions.get::<Router>().unwrap().clone();
    let origin = match params.find("origin") {
        Some(o) => o,
        None => return Ok(Response::with(status::BadRequest)),
    };
    let name = match params.find("name") {
        Some(n) => n,
        None => return Ok(Response::with(status::BadRequest)),
    };
    let vis = match params.find("visibility") {
        Some(v) => v,
        None => return Ok(Response::with(status::BadRequest)),
    };

    // users aren't allowed to set projects to hidden manually
    if vis.to_lowercase() == "hidden" {
        return Ok(Response::with(status::BadRequest));
    }

    let opv: OriginPackageVisibility = match vis.parse() {
        Ok(o) => o,
        Err(_) => return Ok(Response::with(status::BadRequest)),
    };

    if !check_origin_access(req, session.get_id(), &origin)? {
        return Ok(Response::with(status::Forbidden));
    }

    let mut project_get = OriginProjectGet::new();
    project_get.set_name(format!("{}/{}", origin, name));

    match route_message::<OriginProjectGet, OriginProject>(req, &project_get) {
        Ok(mut project) => {
            let real_visibility = transition_visibility(opv, project.get_visibility());
            let mut opu = OriginProjectUpdate::new();
            project.set_visibility(real_visibility);
            opu.set_project(project);

            match route_message::<OriginProjectUpdate, NetOk>(req, &opu) {
                Ok(_) => Ok(Response::with(status::NoContent)),
                Err(err) => Ok(render_net_error(&err)),
            }
        }
        Err(err) => Ok(render_net_error(&err)),
    }
}

pub fn rdeps_show(req: &mut Request) -> IronResult<Response> {
    let mut rdeps_get = ReverseDependenciesGet::new();
    {
        let params = req.extensions.get::<Router>().unwrap();
        match params.find("origin") {
            Some(origin) => rdeps_get.set_origin(origin.to_string()),
            None => return Ok(Response::with(status::BadRequest)),
        }
        match params.find("name") {
            Some(name) => rdeps_get.set_name(name.to_string()),
            None => return Ok(Response::with(status::BadRequest)),
        }
    }

    // TODO (SA): The rdeps API needs to be extended to support a target param.
    // For now, hard code a default value
    rdeps_get.set_target("x86_64-linux".to_string());

    match route_message::<ReverseDependenciesGet, ReverseDependencies>(req, &rdeps_get) {
        Ok(rdeps) => Ok(render_json(status::Ok, &rdeps)),
        Err(err) => return Ok(render_net_error(&err)),
    }
}

pub fn job_show(req: &mut Request) -> IronResult<Response> {
    let session_id = helpers::get_optional_session_id(req);
    let mut request = JobGet::new();
    {
        let params = req.extensions.get::<Router>().unwrap();
        match params.find("id").unwrap().parse::<u64>() {
            Ok(id) => request.set_id(id),
            Err(e) => {
                debug!("Error finding id. e = {:?}", e);
                return Ok(Response::with(status::BadRequest));
            }
        }
    }
    match route_message::<JobGet, Job>(req, &request) {
        Ok(job) => {
            if job.get_package_ident().fully_qualified() {
                let channels =
                    helpers::channels_for_package_ident(req, job.get_package_ident(), session_id);
                let platforms =
                    helpers::platforms_for_package_ident(req, job.get_package_ident(), session_id);
                let mut job_json = serde_json::to_value(job).unwrap();

                if channels.is_some() {
                    job_json["channels"] = json!(channels);
                }

                if platforms.is_some() {
                    job_json["platforms"] = json!(platforms);
                }

                Ok(render_json(status::Ok, &job_json))
            } else {
                Ok(render_json(status::Ok, &job))
            }
        }
        Err(err) => Ok(render_net_error(&err)),
    }
}

pub fn job_log(req: &mut Request) -> IronResult<Response> {
    let session_id = helpers::get_optional_session_id(req);
    let start = {
        let params = req.get_ref::<Params>().unwrap();
        match params.find(&["start"]) {
            Some(&Value::String(ref val)) => {
                match val.parse::<u64>() {
                    Ok(num) => num,
                    Err(e) => {
                        debug!(
                            "Tried to parse 'start' parameter as a number but failed: {:?}",
                            e
                        );
                        return Ok(Response::with(status::BadRequest));
                    }
                }
            }
            _ => 0,
        }
    };

    let include_color = req.get_ref::<Params>()
        .unwrap()
        .find(&["color"])
        .and_then(FromValue::from_value)
        .unwrap_or(false);

    let mut job_get = JobGet::new();
    let mut request = JobLogGet::new();
    request.set_start(start);
    {
        let params = req.extensions.get::<Router>().unwrap();
        match params.find("id").unwrap().parse::<u64>() {
            Ok(id) => {
                request.set_id(id);
                job_get.set_id(id);
            }
            Err(e) => {
                debug!("Error finding id. e = {:?}", e);
                return Ok(Response::with(status::BadRequest));
            }
        }
    }

    // Before fetching the logs, we need to check and see if the logs we want to fetch are for
    // a job that's building a private package, and if so, do we have the right to see said
    // package.
    match route_message::<JobGet, Job>(req, &job_get) {
        Ok(job) => {
            let project = job.get_project();
            if project.get_visibility() == OriginPackageVisibility::Private {
                match session_id {
                    Some(session_id) => {
                        if !check_origin_access(req, session_id, project.get_origin_name())? {
                            return Ok(Response::with(status::Forbidden));
                        }
                    }
                    None => return Ok(Response::with(status::Forbidden)),
                }
            }
            match route_message::<JobLogGet, JobLog>(req, &request) {
                Ok(mut log) => {
                    if !include_color {
                        log.strip_ansi();
                    }
                    Ok(render_json(status::Ok, &log))
                }
                Err(err) => Ok(render_net_error(&err)),
            }
        }
        Err(e) => return Ok(render_net_error(&e)),
    }
}

pub fn notify(req: &mut Request) -> IronResult<Response> {
    if req.headers.has::<XGitHubEvent>() {
        return github::handle_event(req);
    }
    Ok(Response::with(status::BadRequest))
}

/// Endpoint for determining availability of builder-api components.
///
/// Returns a status 200 on success. Any non-200 responses are an outage or a partial outage.
pub fn status(_req: &mut Request) -> IronResult<Response> {
    Ok(Response::with(status::Ok))
}

pub fn list_account_invitations(req: &mut Request) -> IronResult<Response> {
    let mut request = sessionsrv::AccountInvitationListRequest::new();
    {
        let session = req.extensions.get::<Authenticated>().unwrap();
        request.set_account_id(session.get_id());
    }
    match route_message::<
        sessionsrv::AccountInvitationListRequest,
        sessionsrv::AccountInvitationListResponse,
    >(req, &request) {
        Ok(invites) => Ok(render_json(status::Ok, &invites)),
        Err(err) => Ok(render_net_error(&err)),
    }
}

pub fn list_user_origins(req: &mut Request) -> IronResult<Response> {
    let mut request = sessionsrv::AccountOriginListRequest::new();
    {
        let session = req.extensions.get::<Authenticated>().unwrap();
        request.set_account_id(session.get_id());
    }
    match route_message::<
        sessionsrv::AccountOriginListRequest,
        sessionsrv::AccountOriginListResponse,
    >(req, &request) {
        Ok(invites) => Ok(render_json(status::Ok, &invites)),
        Err(err) => Ok(render_net_error(&err)),
    }
}

/// Create a new project as the authenticated user and associated to
/// the given origin.
pub fn project_create(req: &mut Request) -> IronResult<Response> {
    let mut request = OriginProjectCreate::new();
    let mut project = OriginProject::new();
    let mut origin_get = OriginGet::new();
    let github = req.get::<persistent::Read<GitHubCli>>().unwrap();
    let session = req.extensions.get::<Authenticated>().unwrap().clone();
    let session_id = session.get_id();

    let (token, repo_id) = match req.get::<bodyparser::Struct<ProjectCreateReq>>() {
        Ok(Some(body)) => {
            if body.origin.len() <= 0 {
                return Ok(Response::with((
                    status::UnprocessableEntity,
                    "Missing value for field: `origin`",
                )));
            }
            if body.plan_path.len() <= 0 {
                return Ok(Response::with((
                    status::UnprocessableEntity,
                    "Missing value for field: `plan_path`",
                )));
            }

            if !check_origin_access(req, session_id, &body.origin)? {
                return Ok(Response::with(status::Forbidden));
            }

            let token = match github.app_installation_token(body.installation_id) {
                Ok(token) => token,
                Err(err) => {
                    debug!("Error authenticating github app installation, {}", err);
                    return Ok(Response::with(status::Forbidden));
                }
            };

            origin_get.set_name(body.origin);
            project.set_plan_path(body.plan_path);
            project.set_vcs_type(String::from("git"));
            project.set_vcs_installation_id(body.installation_id);

            match github.repo(&token, body.repo_id) {
                Ok(repo) => project.set_vcs_data(repo.clone_url),
                Err(e) => {
                    debug!("Error finding github repo. e = {:?}", e);
                    return Ok(Response::with((status::UnprocessableEntity, "rg:pc:1")));
                }
            }
            (token, body.repo_id)
        }
        _ => return Ok(Response::with(status::UnprocessableEntity)),
    };

    let origin = match route_message::<OriginGet, Origin>(req, &origin_get) {
        Ok(response) => response,
        Err(err) => return Ok(render_net_error(&err)),
    };

    match github.contents(&token, repo_id, &project.get_plan_path()) {
        Ok(contents) => {
            match contents.decode() {
                Ok(bytes) => {
                    match Plan::from_bytes(bytes.as_slice()) {
                        Ok(plan) => {
                            project.set_origin_name(String::from(origin.get_name()));
                            project.set_origin_id(origin.get_id());
                            project.set_package_name(String::from(plan.name.trim_matches('"')));
                        }
                        Err(e) => {
                            debug!("Error matching Plan. e = {:?}", e);
                            return Ok(Response::with((status::UnprocessableEntity, "rg:pc:3")));
                        }
                    }
                }
                Err(e) => {
                    error!("Base64 decode failure: {:?}", e);
                    return Ok(Response::with((status::UnprocessableEntity, "rg:pc:4")));
                }
            }
        }
        Err(e) => {
            debug!("Error fetching contents from GH. e = {:?}", e);
            return Ok(Response::with((status::UnprocessableEntity, "rg:pc:2")));
        }
    }

    project.set_owner_id(session.get_id());
    project.set_visibility(origin.get_default_package_visibility());
    request.set_project(project);
    match route_message::<OriginProjectCreate, OriginProject>(req, &request) {
        Ok(response) => {
            log_event!(
                req,
                Event::ProjectCreate {
                    origin: origin.get_name().to_string(),
                    package: request.get_project().get_id().to_string(),
                    account: session.get_id().to_string(),
                }
            );
            Ok(render_json(status::Created, &response))
        }
        Err(err) => Ok(render_net_error(&err)),
    }
}

/// Delete the given project
pub fn project_delete(req: &mut Request) -> IronResult<Response> {
    let mut project_del = OriginProjectDelete::new();

    let (session_id, origin) = {
        let session = req.extensions.get::<Authenticated>().unwrap();
        let session_id = session.get_id();

        let params = req.extensions.get::<Router>().unwrap();
        let origin = params.find("origin").unwrap().to_owned();
        let name = params.find("name").unwrap();

        project_del.set_name(format!("{}/{}", origin, name));
        (session_id, origin)
    };

    if !check_origin_access(req, session_id, origin)? {
        return Ok(Response::with(status::Forbidden));
    }

    project_del.set_requestor_id(session_id);
    match route_message::<OriginProjectDelete, NetOk>(req, &project_del) {
        Ok(_) => Ok(Response::with(status::NoContent)),
        Err(err) => Ok(render_net_error(&err)),
    }
}

/// Update the given project
pub fn project_update(req: &mut Request) -> IronResult<Response> {
    let (name, origin) = {
        let params = req.extensions.get::<Router>().unwrap();
        let origin = params.find("origin").unwrap().to_owned();
        let name = params.find("name").unwrap().to_owned();

        (name, origin)
    };

    let session_id = {
        let session = req.extensions.get::<Authenticated>().unwrap();
        session.get_id()
    };

    if !check_origin_access(req, session_id, &origin)? {
        return Ok(Response::with(status::Forbidden));
    }

    let mut project_get = OriginProjectGet::new();
    project_get.set_name(format!("{}/{}", &origin, &name));
    let mut project = match route_message::<OriginProjectGet, OriginProject>(req, &project_get) {
        Ok(project) => project,
        Err(err) => return Ok(render_net_error(&err)),
    };

    let mut request = OriginProjectUpdate::new();
    let github = req.get::<persistent::Read<GitHubCli>>().unwrap();

    let (token, repo_id) = match req.get::<bodyparser::Struct<ProjectCreateReq>>() {
        Ok(Some(body)) => {
            if body.plan_path.len() <= 0 {
                return Ok(Response::with((
                    status::UnprocessableEntity,
                    "Missing value for field: `plan_path`",
                )));
            }

            let token = match github.app_installation_token(body.installation_id) {
                Ok(token) => token,
                Err(err) => {
                    debug!("Error authenticating github app installation, {}", err);
                    return Ok(Response::with(status::Forbidden));
                }
            };

            project.set_plan_path(body.plan_path);
            project.set_vcs_installation_id(body.installation_id);
            match github.repo(&token, body.repo_id) {
                Ok(repo) => project.set_vcs_data(repo.clone_url),
                Err(e) => {
                    debug!("Error finding GH repo. e = {:?}", e);
                    return Ok(Response::with((status::UnprocessableEntity, "rg:pu:1")));
                }
            }
            (token, body.repo_id)
        }
        _ => return Ok(Response::with(status::UnprocessableEntity)),
    };

    match github.contents(&token, repo_id, &project.get_plan_path()) {
        Ok(contents) => {
            match base64::decode(&contents.content) {
                Ok(bytes) => {
                    match Plan::from_bytes(bytes.as_slice()) {
                        Ok(plan) => {
                            if plan.name != name {
                                return Ok(Response::with((status::UnprocessableEntity, "rg:pu:2")));
                            }
                            project.set_origin_name(String::from(origin));
                            project.set_package_name(String::from(name));
                        }
                        Err(e) => {
                            debug!("Error matching Plan. e = {:?}", e);
                            return Ok(Response::with((status::UnprocessableEntity, "rg:pu:3")));
                        }
                    }
                }
                Err(e) => {
                    debug!("Error decoding content from b64. e = {:?}", e);
                    return Ok(Response::with((status::UnprocessableEntity, "rg:pu:4")));
                }
            }
        }
        Err(e) => {
            debug!("Erroring fetching contents from GH. e = {:?}", e);
            return Ok(Response::with((status::UnprocessableEntity, "rg:pu:5")));
        }
    }

    request.set_requestor_id(session_id);
    request.set_project(project);
    match route_message::<OriginProjectUpdate, NetOk>(req, &request) {
        Ok(_) => Ok(Response::with(status::NoContent)),
        Err(err) => Ok(render_net_error(&err)),
    }
}

/// Display the the given project's details
pub fn project_show(req: &mut Request) -> IronResult<Response> {
    let mut project_get = OriginProjectGet::new();

    let (session_id, origin) = {
        let session = req.extensions.get::<Authenticated>().unwrap();
        let session_id = session.get_id();

        let params = req.extensions.get::<Router>().unwrap();
        let origin = params.find("origin").unwrap().to_owned();
        let name = params.find("name").unwrap();

        project_get.set_name(format!("{}/{}", origin, name));
        (session_id, origin)
    };

    if !check_origin_access(req, session_id, &origin)? {
        return Ok(Response::with(status::Forbidden));
    }

    match route_message::<OriginProjectGet, OriginProject>(req, &project_get) {
        Ok(project) => Ok(render_json(status::Ok, &project)),
        Err(err) => Ok(render_net_error(&err)),
    }
}

/// Return names of all the projects in the given origin
pub fn project_list(req: &mut Request) -> IronResult<Response> {
    let mut projects_get = OriginProjectListGet::new();

    let (session_id, origin) = {
        let session = req.extensions.get::<Authenticated>().unwrap();
        let session_id = session.get_id();

        let params = req.extensions.get::<Router>().unwrap();
        let origin = params.find("origin").unwrap().to_owned();

        projects_get.set_origin(origin.to_string());
        (session_id, origin)
    };

    if !check_origin_access(req, session_id, &origin)? {
        return Ok(Response::with(status::Forbidden));
    }

    match route_message::<OriginProjectListGet, OriginProjectList>(req, &projects_get) {
        Ok(projects) => Ok(render_json(status::Ok, &projects.get_names())),
        Err(err) => Ok(render_net_error(&err)),
    }
}

/// Retrieve the most recent 50 jobs for a project.
pub fn project_jobs(req: &mut Request) -> IronResult<Response> {
    let mut jobs_get = ProjectJobsGet::new();

    let (session_id, origin) = {
        let session = req.extensions.get::<Authenticated>().unwrap();
        let session_id = session.get_id();

        let params = req.extensions.get::<Router>().unwrap();
        let origin = params.find("origin").unwrap().to_owned();
        let name = params.find("name").unwrap();
        jobs_get.set_name(format!("{}/{}", origin, name));

        (session_id, origin)
    };

    if !check_origin_access(req, session_id, &origin)? {
        return Ok(Response::with(status::Forbidden));
    }

    match helpers::extract_pagination(req) {
        Ok((start, stop)) => {
            jobs_get.set_start(start as u64);
            jobs_get.set_stop(stop as u64);
        }
        Err(response) => return Ok(response),
    }
    match route_message::<ProjectJobsGet, ProjectJobsGetResponse>(req, &jobs_get) {
        Ok(response) => {
            let list: Vec<serde_json::Value> = response
                .get_jobs()
                .iter()
                .map(|job| if job.get_state() == JobState::Complete {
                    let channels = helpers::channels_for_package_ident(
                        req,
                        &job.get_package_ident(),
                        Some(session_id),
                    );
                    let platforms = helpers::platforms_for_package_ident(
                        req,
                        &job.get_package_ident(),
                        Some(session_id),
                    );
                    let mut job_json = serde_json::to_value(job).unwrap();

                    if channels.is_some() {
                        job_json["channels"] = json!(channels);
                    }

                    if platforms.is_some() {
                        job_json["platforms"] = json!(platforms);
                    }

                    job_json
                } else {
                    serde_json::to_value(job).unwrap()
                })
                .collect();

            helpers::paginated_response(
                &list,
                response.get_count() as isize,
                response.get_start() as isize,
                response.get_stop() as isize,
            )
        }
        Err(err) => Ok(render_net_error(&err)),
    }
}

pub fn create_project_integration(req: &mut Request) -> IronResult<Response> {
    let params = match validate_params(req, &["origin", "name", "integration"]) {
        Ok(p) => p,
        Err(st) => return Ok(Response::with(st)),
    };

    let body = req.get::<bodyparser::Json>();
    match body {
        Ok(Some(_)) => (),
        Ok(None) => {
            warn!("create_project_integration: Empty body in request");
            return Ok(Response::with(status::BadRequest));
        }
        Err(e) => {
            warn!("create_project_integration, Error parsing body: {:?}", e);
            return Ok(Response::with(status::BadRequest));
        }
    };

    // We know body exists and is valid, non-empty JSON, so we can unwrap safely
    let json_body = req.get::<bodyparser::Raw>().unwrap().unwrap();

    let mut opi = OriginProjectIntegration::new();
    opi.set_origin(params["origin"].clone());
    opi.set_name(params["name"].clone());
    opi.set_integration(params["integration"].clone());
    opi.set_integration_name(String::from(DEFAULT_PROJECT_INTEGRATION));
    opi.set_body(json_body);

    let mut request = OriginProjectIntegrationCreate::new();
    request.set_integration(opi);

    match route_message::<OriginProjectIntegrationCreate, NetOk>(req, &request) {
        Ok(_) => Ok(Response::with(status::NoContent)),
        Err(err) => {
            if err.get_code() == ErrCode::ENTITY_CONFLICT {
                warn!("Failed to create integration as it already exists");
                Ok(Response::with(status::Conflict))
            } else {
                error!("create_project_integration:1, err={:?}", err);
                Ok(Response::with(status::InternalServerError))
            }
        }
    }
}

pub fn get_project_integration(req: &mut Request) -> IronResult<Response> {
    let params = match validate_params(req, &["origin", "name", "integration"]) {
        Ok(p) => p,
        Err(st) => return Ok(Response::with(st)),
    };

    let mut opi = OriginProjectIntegration::new();
    opi.set_origin(params["origin"].clone());
    opi.set_name(params["name"].clone());
    opi.set_integration(params["integration"].clone());
    opi.set_integration_name(String::from(DEFAULT_PROJECT_INTEGRATION));

    let mut request = OriginProjectIntegrationGet::new();
    request.set_integration(opi);

    match route_message::<OriginProjectIntegrationGet, OriginProjectIntegration>(req, &request) {
        Ok(integration) => {
            let v: serde_json::Value = match serde_json::from_str(&integration.get_body()) {
                Ok(v) => v,
                Err(e) => {
                    debug!("Error parsing to JSON. e = {:?}", e);
                    return Ok(Response::with((status::UnprocessableEntity, "api:gpi:1")));
                }
            };
            Ok(render_json(status::Ok, &v))
        }
        Err(err) => {
            match err.get_code() {
                ErrCode::ENTITY_NOT_FOUND => Ok(Response::with((status::NotFound))),
                _ => {
                    error!(
                        "Unexpected error retrieving project integration, err={:?}",
                        err
                    );
                    Ok(Response::with((status::InternalServerError, "api:gpi:2")))
                }
            }
        }
    }
}
