//! Ordered tenant email delivery. A trusted bridge service uses its database
//! identity to resolve immutable TicketCreated records and mint narrowly scoped
//! command tokens. The gateway remains responsible for sender authentication.
use crate::{
    email_channel::{self, EmailCommand, InboundEmail},
    helpdesk::BOUNDED_CONTEXT,
    tenant_access::recorded_company_tenants,
};
use futures_util::TryStreamExt;
use sha2::{Digest, Sha256};
use skilj_core::{
    access_control::{self, AccessLevel, RoleStatus},
    db,
    shared::Tag,
};
use std::time::Duration;

pub struct TenantEmail {
    pub pool: db::Pool,
    pub role_id: String,
    /// A deployment secret, reused across restarts so idempotency keeps the
    /// same submitting token identity. Supply 64 random hex characters.
    pub token_seed: String,
    pub base_url: String,
    pub http: reqwest::Client,
}

impl TenantEmail {
    pub async fn destination(&self, command: &EmailCommand) -> Result<String, String> {
        let tenants = recorded_company_tenants(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        let ticket_id = match command {
            EmailCommand::CreateTicket(payload) => &payload.ticket_id,
            EmailCommand::CustomerReply(payload) => &payload.ticket_id,
        };
        let active_contexts: std::collections::HashSet<_> = db::list_bounded_contexts(&self.pool)
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|bc| bc.status == skilj_core::event_store::BoundedContextStatus::Active)
            .map(|bc| bc.name)
            .collect();
        let mut contexts: Vec<_> = tenants.iter().map(|(_, tenant)| tenant.clone()).collect();
        contexts.push(BOUNDED_CONTEXT.into());
        contexts.sort();
        contexts.dedup();
        let mut found = Vec::new();
        for context in contexts.into_iter().filter(|c| active_contexts.contains(c)) {
            let events = self.ticket_events(&context, ticket_id).await?;
            for event in events
                .iter()
                .filter(|e| e.event_type.name == "TicketCreated")
            {
                let payload: serde_json::Value =
                    serde_json::from_str(&event.payload).map_err(|e| e.to_string())?;
                if let EmailCommand::CreateTicket(create) = command {
                    if payload["company_id"].as_str() != Some(&create.company_id)
                        || payload["requester_id"].as_str() != Some(&create.requester_id)
                    {
                        return Err(
                            "Message-ID already belongs to another company or requester".into()
                        );
                    }
                }
                found.push(context.clone());
            }
        }
        match found.as_slice() {
            [context] => Ok(context.clone()),
            [] => match command {
                EmailCommand::CreateTicket(payload) => tenants
                    .iter()
                    .find(|(company, _)| company == &payload.company_id)
                    .map(|(_, tenant)| tenant.clone())
                    .ok_or_else(|| "company has no recorded tenant yet".into()),
                EmailCommand::CustomerReply(_) => Err("ticket not found".into()),
            },
            _ => Err("ambiguous ticket id across contexts; refusing to guess".into()),
        }
    }

    async fn ticket_events(
        &self,
        context: &str,
        ticket_id: &str,
    ) -> Result<Vec<skilj_core::event_store::Event>, String> {
        db::list_events_for_bounded_context_matching_tags(
            &self.pool,
            context,
            &[Tag {
                key: "ticket".into(),
                value: Some(ticket_id.into()),
            }],
            None,
        )
        .await
        .map_err(|e| e.to_string())
    }

    pub async fn mapping(
        &self,
        context: &str,
        command: &EmailCommand,
    ) -> Result<skilj_nats::InboundMapping, String> {
        let role = db::get_role(&self.pool, &self.role_id)
            .await
            .map_err(|e| e.to_string())?
            .filter(|r| r.status == RoleStatus::Active)
            .ok_or("email service role is inactive")?;
        let mapping = db::get_active_role_access_mapping(&self.pool, &role.id, context)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("email service has no tenant grant")?;
        if mapping.level != AccessLevel::Admin {
            return Err("email service needs an Admin tenant grant to mint tokens".into());
        }
        let command_name = match command {
            EmailCommand::CreateTicket(_) => "CreateTicket",
            EmailCommand::CustomerReply(_) => "CustomerRespondsToTicket",
        };
        let identity = serde_json::to_vec(&(&role.id, context, command_name)).unwrap();
        let id = hex(&Sha256::digest(&identity))[..32].to_owned();
        let mut digest = Sha256::new();
        digest.update(&identity);
        digest.update(self.token_seed.as_bytes());
        let secret = hex(&digest.finalize());
        match db::get_command_token(&self.pool, &id)
            .await
            .map_err(|e| e.to_string())?
        {
            Some(token) => {
                if token.status != access_control::TokenStatus::Active
                    || token.scope != mapping.scope
                    || token.secret != skilj_core::shared::hash_secret(&secret)
                {
                    return Err(
                        "email token revoked or seed changed; operator intervention required"
                            .into(),
                    );
                }
            }
            None => {
                let kind = db::get_command_type(&self.pool, context, command_name)
                    .await
                    .map_err(|e| e.to_string())?
                    .ok_or("command is not registered")?;
                let token = access_control::create_command_token(
                    &mapping,
                    &kind,
                    id.clone(),
                    secret.clone(),
                    mapping.scope.clone(),
                    chrono::Utc::now(),
                )
                .map_err(|e| e.to_string())?;
                db::insert_command_token(&self.pool, &token)
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
        Ok(skilj_nats::InboundMapping {
            credential: format!("{id}.{secret}"),
            action: skilj_nats::InboundAction::Trigger {
                command_type: command_name.into(),
            },
            blobs: None,
        })
    }

    pub async fn deliver(&self, command: &EmailCommand, key: &str) -> Result<(), String> {
        let context = self.destination(command).await?;
        let mapping = self.mapping(&context, command).await?;
        let ticket_id = match command {
            EmailCommand::CreateTicket(payload) => &payload.ticket_id,
            EmailCommand::CustomerReply(payload) => &payload.ticket_id,
        };
        if self
            .ticket_events(&context, ticket_id)
            .await?
            .iter()
            .any(|event| event.metadata.correlation_id.as_deref() == Some(key))
        {
            return Ok(());
        }
        let request = serde_json::json!({"payload": command.payload_json()});
        let first_failed = chrono::Utc::now();
        let retry = skilj_retry::RetryPolicy::default();
        for attempt in 1..=5 {
            let result = self
                .http
                .post(format!("{}/v1/commands/trigger", self.base_url))
                .bearer_auth(&mapping.credential)
                .header("Idempotency-Key", key)
                .header("Skilj-Correlation-Id", key)
                .json(&request)
                .send()
                .await;
            let error = match result {
                Ok(response) if response.status().is_success() => return Ok(()),
                Ok(response) => format!(
                    "HTTP {}: {}",
                    response.status(),
                    response.text().await.unwrap_or_default()
                ),
                Err(error) => error.to_string(),
            };
            if attempt == 5 {
                // The server stores enough to redrive the exact command.
                let response = self.http.post(format!("{}/v1/parked-deliveries", self.base_url))
                    .bearer_auth(&mapping.credential).json(&serde_json::json!({
                        "source": "nats-inbound", "kind": mapping.action.parked_delivery_kind(), "identifier": key,
                        "error": error, "attemptCount": attempt, "firstFailedAt": first_failed,
                        "request": request, "idempotencyKey": key,
                    })).send().await.map_err(|e| e.to_string())?;
                response.error_for_status().map_err(|e| e.to_string())?;
                return Ok(());
            }
            tokio::time::sleep(retry.next_backoff(attempt)).await;
        }
        unreachable!()
    }
}

pub async fn run_until(
    service: TenantEmail,
    stream: &async_nats::jetstream::stream::Stream,
    jetstream: &async_nats::jetstream::Context,
    stop: impl std::future::Future<Output = ()>,
) -> Result<(), async_nats::Error> {
    // A single outstanding message across instances, including retries. A
    // reply cannot overtake an earlier create while routing is unavailable.
    let consumer = stream
        .get_or_create_consumer(
            "email-tenant-delivery",
            async_nats::jetstream::consumer::pull::Config {
                durable_name: Some("email-tenant-delivery".into()),
                filter_subject: email_channel::RECEIVED_SUBJECT.into(),
                max_ack_pending: 1,
                ack_wait: Duration::from_secs(300),
                ..Default::default()
            },
        )
        .await?;
    let mut stop = std::pin::pin!(stop);
    loop {
        let mut messages = consumer.messages().await?;
        loop {
            let message = tokio::select! {
                () = &mut stop => return Ok(()),
                message = messages.try_next() => match message? { Some(m) => m, None => break },
            };
            let command = serde_json::from_slice::<InboundEmail>(&message.payload)
                .map_err(|e| e.to_string())
                .and_then(|email| {
                    email_channel::route(&email)
                        .map(|command| (email_channel::email_key(&email.message_id), command))
                        .map_err(|e| e.to_string())
                });
            match command {
                Err(reason) => {
                    email_channel::publish_unroutable(jetstream, &message, &reason).await?
                }
                Ok((key, command)) => loop {
                    match service.deliver(&command, &key).await {
                        Ok(()) => break,
                        Err(error)
                            if error == "ticket not found"
                                || error.starts_with("ambiguous ticket id")
                                || error.starts_with("Message-ID already belongs") =>
                        {
                            email_channel::publish_unroutable(jetstream, &message, &error).await?;
                            break;
                        }
                        Err(error) => {
                            tracing::error!(%error, "tenant email delivery blocked; retrying in order")
                        }
                    }
                    let _ = message
                        .ack_with(async_nats::jetstream::AckKind::Progress)
                        .await;
                    tokio::select! {
                        () = &mut stop => return Ok(()),
                        () = tokio::time::sleep(Duration::from_secs(1)) => {},
                    }
                },
            }
            message.ack().await?;
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
