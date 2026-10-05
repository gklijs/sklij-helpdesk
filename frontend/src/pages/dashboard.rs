use crate::{
    api, auth, config,
    model::{CustomerTicketContent, TicketInternalNote, TicketQueueEntry},
    routing::TicketContext,
    theme::ThemeToggle,
};
use leptos::ev::SubmitEvent;
use leptos::prelude::*;
use leptos::task::spawn_local;
use std::collections::HashMap;
use std::time::Duration;
use web_sys::window;

/// Refetches now, and again once skilj's background catch-up has had a
/// tick to fold the write in. `CompanyActiveTickets` is async (issue #15),
/// so the immediate refetch usually still misses what was just
/// written; `CustomerTickets` is sync and is already current. 750ms is
/// skilj's default `async_projection_poll_interval` (500ms) plus room
/// for the fold itself.
fn refresh_after_write(set_refresh: WriteSignal<u32>) {
    set_refresh.update(|n| *n += 1);
    set_timeout(
        move || set_refresh.update(|n| *n += 1),
        Duration::from_millis(750),
    );
}

/// `TicketInternalNotes` is its own projection, queried on demand, not
/// as part of the eager ticket fetch `Dashboard` already does - see that projection's own doc comment in
/// `helpdesk.rs`. Shared between the "Notes" toggle's own first fetch
/// and `AddInternalNote`'s own re-fetch-after-success, rather than
/// duplicated inline in each.
///
/// Takes the resolved `TicketContext` rather than naming a context
/// itself: the notes projection is keyed by `ticket_id`, so nothing in
/// this request identifies the company, and the context has to come from
/// the one resolution the dashboard already did. Passing it down is what
/// keeps this read on the same tenant as the `AddInternalNote` write
/// that precedes it.
async fn fetch_internal_notes(
    token: &str,
    ticket_context: &TicketContext,
    ticket_id: &str,
) -> Result<Vec<TicketInternalNote>, String> {
    let json = api::query_projection(
        token,
        ticket_context.bounded_context(),
        "TicketInternalNotes",
        ticket_id,
        &ticket_context.graphql_type("TicketInternalNotes"),
        "notes",
    )
    .await?;
    serde_json::from_value(json).map_err(|e| e.to_string())
}

/// One customer's `CustomerTickets` row: the content of each of their
/// tickets, by `ticket_id`. Keyed by the customer's `requester_id`, which
/// for a logged-in customer is their own `sub` - that's what lets skilj
/// decrypt it for them without any sensitive-read grant.
async fn fetch_customer_tickets(
    token: &str,
    ticket_context: &TicketContext,
    requester_id: &str,
) -> Result<HashMap<String, CustomerTicketContent>, String> {
    let json = api::query_projection(
        token,
        ticket_context.bounded_context(),
        "CustomerTickets",
        requester_id,
        &ticket_context.graphql_type("CustomerTickets"),
        "tickets",
    )
    .await?;
    serde_json::from_value(json).map_err(|e| e.to_string())
}

/// Dex's own opaque `sub` values (see `auth::decode_jwt_sub`'s own doc
/// comment on why they're not the plain userID) are ~30 characters of
/// base64 - fine to authenticate with, unreadable to show a person.
/// Shortened for display only; every command payload still sends the
/// real, full id.
fn short_id(id: &str) -> String {
    if id.chars().count() > 10 {
        format!("{}…", id.chars().take(8).collect::<String>())
    } else {
        id.to_string()
    }
}

/// Midnight UTC of "today" - `specs/activity.allium`'s own
/// `DailyActivityRecorded.day` comment: calendar-day granularity, no
/// dedicated Date primitive to reach for, caller-truncated. No `chrono`
/// in this crate (see `Cargo.toml`'s own dependency list); `js_sys::Date::UTC`
/// only takes `(year, month)` in this version, defaulting to the 1st of
/// the month, not today - so this floors `Date::now()`'s own
/// milliseconds-since-epoch to the current UTC day's own start instead,
/// which is correct regardless of the browser's local timezone (epoch
/// millis are UTC by definition).
const MILLIS_PER_DAY: f64 = 86_400_000.0;

fn today_midnight_utc() -> String {
    let day_start_millis = (js_sys::Date::now() / MILLIS_PER_DAY).floor() * MILLIS_PER_DAY;
    String::from(js_sys::Date::new(&day_start_millis.into()).to_iso_string())
}

#[component]
pub fn Dashboard() -> impl IntoView {
    let Some((token, role)) = auth::current_session() else {
        Effect::new(move |_| {
            let _ = window().expect("browser").location().set_href("/login");
        });
        return view! { <p>"Redirecting to login..."</p> }.into_any();
    };

    let my_sub = auth::decode_jwt_sub(&token).unwrap_or_default();
    let is_staff = matches!(role, auth::Role::StaffLead);

    // `specs/activity.allium`'s own `surface CustomerActivityPing`/
    // `StaffActivityPing`: called unconditionally on every dashboard
    // load, customer and staff alike, letting `decide()` do the
    // once-per-company-per-person-per-day throttling
    // (`already_recorded_today`) - not tied to `refresh` below, so this
    // fires once per page load, not once per ticket action. A rejection
    // here is the expected steady state after the first load of the
    // day, not an error worth surfacing in `status`.
    {
        let token = token.clone();
        let my_sub = my_sub.clone();
        Effect::new(move |_| {
            let payload = serde_json::json!({
                "company_id": config::DEMO_COMPANY_ID,
                "person_kind": if is_staff { "staff" } else { "customer" },
                "person_subject": my_sub,
                "day": today_midnight_utc(),
            });
            let token = token.clone();
            spawn_local(async move {
                let _ = api::submit_command(
                    &token,
                    config::ACTIVITY_BOUNDED_CONTEXT,
                    "RecordDailyActivity",
                    &payload,
                )
                .await;
            });
        });
    }

    // `api::query_projection` already resolves down to the `tickets`
    // field's own inner JSON (a plain ticket_id -> entry map, per
    // `CompanyActiveTicketsState`'s own shape on the backend) - deserialize
    // into that map directly, not the struct that wraps it. Found by
    // running the real thing in a real browser: every fetch failed with
    // "missing field `tickets`", not intermittently - an earlier
    // `…State` target here was double-unwrapping.
    let (tickets, set_tickets) = signal(HashMap::<String, TicketQueueEntry>::new());
    // Each ticket's content, from its customer's `CustomerTickets` row.
    let (contents, set_contents) = signal(HashMap::<String, CustomerTicketContent>::new());
    let (status, set_status) = signal(String::new());
    let (refresh, set_refresh) = signal(0u32);

    // Where this company's Ticket traffic belongs, resolved once per
    // session and then read by every ticket call below. Resolving here
    // rather than per call is the point: reads and writes have to agree
    // on the context or the dashboard shows one company's history while
    // its writes land in another.
    //
    // Starts as `Resolving` and the fetch below waits on it, so the
    // first load of a session can't read the shared context before the
    // answer is known. Every ticket action is also gated on it - see
    // `on_create` - because a write sent before resolution finished
    // would be the one request the server refuses as misrouted.
    let (ticket_context, set_ticket_context) = signal(TicketContext::Resolving);
    {
        let token = token.clone();
        spawn_local(async move {
            set_ticket_context.set(crate::routing::resolve(&token).await);
        });
    }

    let fetch_token = token.clone();
    let fetch_sub = my_sub.clone();
    Effect::new(move |_| {
        refresh.get();
        // Track the resolved context as well as the refresh counter, so
        // the first fetch happens once resolution lands rather than
        // firing immediately against an unknown context.
        let context = ticket_context.get();
        if context == TicketContext::Resolving {
            return;
        }
        let token = fetch_token.clone();
        let my_sub = fetch_sub.clone();
        spawn_local(async move {
            let result = api::query_projection(
                &token,
                context.bounded_context(),
                "CompanyActiveTickets",
                config::DEMO_COMPANY_ID,
                &context.graphql_type("CompanyActiveTickets"),
                "tickets",
            )
            .await
            .and_then(|json| {
                serde_json::from_value::<HashMap<String, TicketQueueEntry>>(json)
                    .map_err(|e| e.to_string())
            });
            let queue = match result {
                Ok(queue) => queue,
                Err(e) => {
                    set_status.set(format!("couldn't load tickets: {e}"));
                    return;
                }
            };
            // Content is per customer: staff read one row per customer in
            // the queue, a customer only their own. One request each, in
            // turn - fine for a demo company's handful of customers, and
            // the price of keeping each customer's text under their own key.
            //
            // Only customers the queue shows a ticket for: a row that
            // doesn't exist yet has no owner, so a company-scoped
            // customer's read of it is refused (`grant_scope_mismatch`).
            // The queue read above has the same limit before a company's
            // first ticket.
            let mut requesters: Vec<String> = queue
                .values()
                .map(|t| t.requester_id.clone())
                .filter(|requester_id| is_staff || *requester_id == my_sub)
                .collect();
            requesters.sort();
            requesters.dedup();
            set_tickets.set(queue);
            let mut all = HashMap::new();
            for requester_id in requesters {
                match fetch_customer_tickets(&token, &context, &requester_id).await {
                    Ok(content) => all.extend(content),
                    Err(e) => set_status.set(format!("couldn't load ticket content: {e}")),
                }
            }
            set_contents.set(all);
        });
    });

    let (new_title, set_new_title) = signal(String::new());
    let (new_description, set_new_description) = signal(String::new());
    let (new_priority, set_new_priority) = signal("low".to_string());

    let create_token = token.clone();
    let create_requester = my_sub.clone();
    let on_create = move |ev: SubmitEvent| {
        ev.prevent_default();
        // Refused while the context is still unknown rather than
        // defaulted to the shared context: this is the one request that
        // could otherwise open a session by writing to the wrong place.
        let context = ticket_context.get();
        if context == TicketContext::Resolving {
            set_status
                .set("still working out which context to use - try again in a moment".to_string());
            return;
        }
        let token = create_token.clone();
        let requester_id = create_requester.clone();
        let title = new_title.get();
        let description = new_description.get();
        let priority = new_priority.get();
        spawn_local(async move {
            let ticket_id = format!("tk-{}", js_sys::Date::now() as u64);
            let payload = serde_json::json!({
                "ticket_id": ticket_id,
                "company_id": config::DEMO_COMPANY_ID,
                "requester_id": requester_id,
                "logged_by_staff_id": Option::<String>::None,
                "title": title,
                "description": description,
                "priority": priority,
            });
            match api::submit_command(&token, context.bounded_context(), "CreateTicket", &payload)
                .await
            {
                Ok(_) => refresh_after_write(set_refresh),
                Err(e) => set_status.set(format!("couldn't create ticket: {e}")),
            }
        });
    };

    let log_out = move |_| {
        auth::log_out();
        let _ = window().expect("browser").location().set_href("/login");
    };

    view! {
        <nav>
            <h1>"SkilJ Helpdesk"</h1>
            <div class="nav-right">
                <span>{if is_staff { "Staff view" } else { "Customer view" }} " — " {short_id(&my_sub)}</span>
                <ThemeToggle/>
                <button on:click=log_out>"Log out"</button>
            </div>
        </nav>
        <p class="error">{move || status.get()}</p>

        <Show when=move || !is_staff>
            <h2>"Create a ticket"</h2>
            <form on:submit=on_create.clone()>
                <input
                    type="text"
                    placeholder="Title"
                    prop:value=new_title
                    on:input:target=move |ev| set_new_title.set(ev.target().value())
                />
                <textarea
                    placeholder="Description"
                    prop:value=new_description
                    on:input:target=move |ev| set_new_description.set(ev.target().value())
                ></textarea>
                <select on:change:target=move |ev| set_new_priority.set(ev.target().value())>
                    <option value="low">"Low"</option>
                    <option value="medium">"Medium"</option>
                    <option value="high">"High"</option>
                    <option value="urgent">"Urgent"</option>
                </select>
                <button type="submit">"Create"</button>
            </form>
        </Show>

        <h2>"Tickets"</h2>
        <div class="table-wrap">
        <table>
            <thead>
                <tr>
                    <th>"Title"</th>
                    <th>"Status"</th>
                    <th>"Priority"</th>
                    <th>"Requester"</th>
                    <th>"Assigned"</th>
                    <th>"Actions"</th>
                </tr>
            </thead>
            <tbody>
                {move || {
                    let my_sub = my_sub.clone();
                    let token = token.clone();
                    let contents = contents.get();
                    let mut entries: Vec<TicketQueueEntry> = tickets
                        .get()
                        .into_values()
                        .filter(|t| is_staff || t.requester_id == my_sub)
                        .collect();
                    entries.sort_by(|a, b| a.ticket_id.cmp(&b.ticket_id));
                    entries
                        .into_iter()
                        .map(|ticket| {
                            view! {
                                <TicketRow
                                    content=contents.get(&ticket.ticket_id).cloned()
                                    ticket=ticket
                                    is_staff=is_staff
                                    my_sub=my_sub.clone()
                                    token=token.clone()
                                    ticket_context=ticket_context.get()
                                    set_refresh=set_refresh
                                    set_status=set_status
                                />
                            }
                        })
                        .collect_view()
                }}
            </tbody>
        </table>
        </div>
    }
    .into_any()
}

#[component]
fn TicketRow(
    ticket: TicketQueueEntry,
    /// `None` until it loads, or when it can't be read at all.
    content: Option<CustomerTicketContent>,
    is_staff: bool,
    my_sub: String,
    token: String,
    ticket_context: TicketContext,
    set_refresh: WriteSignal<u32>,
    set_status: WriteSignal<String>,
) -> impl IntoView {
    let ticket_id = ticket.ticket_id.clone();
    // Clones `token` for `run`'s own environment rather than moving the
    // outer parameter in directly - `toggle_notes`/`add_note` below need
    // their own copy of it too.
    let run = {
        let token = token.clone();
        let ticket_context = ticket_context.clone();
        move |command_type_name: &'static str, payload: serde_json::Value| {
            // Same gate as `on_create`: a ticket action taken before the
            // context resolved must not default to the shared context.
            if ticket_context == TicketContext::Resolving {
                set_status.set(
                    "still working out which context to use - try again in a moment".to_string(),
                );
                return;
            }
            let token = token.clone();
            let ticket_context = ticket_context.clone();
            spawn_local(async move {
                match api::submit_command(
                    &token,
                    ticket_context.bounded_context(),
                    command_type_name,
                    &payload,
                )
                .await
                {
                    Ok(_) => refresh_after_write(set_refresh),
                    Err(e) => set_status.set(format!("{command_type_name} failed: {e}")),
                }
            });
        }
    };

    let assign = {
        let ticket_id = ticket_id.clone();
        let my_sub = my_sub.clone();
        let run = run.clone();
        move |_| {
            run(
                "AssignTicket",
                serde_json::json!({ "ticket_id": ticket_id, "staff_id": my_sub }),
            )
        }
    };
    let resolve = {
        let ticket_id = ticket_id.clone();
        let run = run.clone();
        move |_| {
            run(
                "ResolveTicket",
                serde_json::json!({ "ticket_id": ticket_id }),
            )
        }
    };
    let close = {
        let ticket_id = ticket_id.clone();
        let run = run.clone();
        move |_| run("CloseTicket", serde_json::json!({ "ticket_id": ticket_id }))
    };
    let reopen = {
        let ticket_id = ticket_id.clone();
        let run = run.clone();
        move |_| {
            run(
                "ReopenTicket",
                serde_json::json!({ "ticket_id": ticket_id }),
            )
        }
    };

    // The one round of `StaffRequestsInfo`/`CustomerReplies` actually
    // relevant to whoever's looking at this row - staff can ask
    // (in_progress), the ticket's own requester can answer
    // (waiting_on_customer). Shares one text signal since only one of
    // the two is ever visible for a given (role, status) combination.
    let (message_text, set_message_text) = signal(String::new());
    let is_own_ticket = ticket.requester_id == my_sub;

    let ask = {
        let ticket_id = ticket_id.clone();
        let my_sub = my_sub.clone();
        let requester_id = ticket.requester_id.clone();
        let run = run.clone();
        move |_| {
            let text = message_text.get();
            set_message_text.set(String::new());
            // `requester_id` names the key the question is encrypted under.
            run(
                "RequestInfoFromCustomer",
                serde_json::json!({
                    "ticket_id": ticket_id, "staff_id": my_sub, "message": text,
                    "requester_id": requester_id,
                }),
            )
        }
    };
    let reply = {
        let ticket_id = ticket_id.clone();
        let my_sub = my_sub.clone();
        let run = run.clone();
        move |_| {
            let text = message_text.get();
            set_message_text.set(String::new());
            run(
                "CustomerRespondsToTicket",
                serde_json::json!({ "ticket_id": ticket_id, "requester_id": my_sub, "message": text }),
            )
        }
    };

    // CSAT (RateTicket) - customer-only, own ticket, once resolved/closed,
    // and only while `ticket.rating` is still absent (see
    // `TicketSummaryState::rating`'s own doc comment for why that field
    // exists at all: exactly so this form knows when to stop showing
    // itself).
    let (rating_value, set_rating_value) = signal(5u32);
    let (rating_comment, set_rating_comment) = signal(String::new());
    let can_rate = !is_staff
        && is_own_ticket
        && matches!(ticket.status.as_str(), "resolved" | "closed")
        && ticket.rating.is_none();
    let existing_rating = ticket.rating;
    let submit_rating = {
        let ticket_id = ticket_id.clone();
        let my_sub = my_sub.clone();
        let run = run.clone();
        move |_| {
            let rating = rating_value.get();
            let comment = rating_comment.get();
            set_rating_comment.set(String::new());
            let comment = if comment.trim().is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::Value::String(comment)
            };
            run(
                "RateTicket",
                serde_json::json!({
                    "ticket_id": ticket_id, "rating": rating, "comment": comment,
                    "requester_id": my_sub,
                }),
            )
        }
    };

    // Internal notes - staff-only, fetched on demand (a second, separate
    // query - see `fetch_internal_notes`'s own doc comment for why this
    // never rides along with the eager ticket fetch).
    let (note_text, set_note_text) = signal(String::new());
    let (notes, set_notes) = signal(Vec::<TicketInternalNote>::new());
    let (notes_open, set_notes_open) = signal(false);
    let toggle_notes = {
        let ticket_id = ticket_id.clone();
        let token = token.clone();
        let ticket_context = ticket_context.clone();
        move |_| {
            let opening = !notes_open.get();
            set_notes_open.set(opening);
            if opening {
                let ticket_id = ticket_id.clone();
                let token = token.clone();
                let ticket_context = ticket_context.clone();
                spawn_local(async move {
                    if ticket_context == TicketContext::Resolving {
                        set_status.set(
                            "still working out which context to use - try again in a moment"
                                .to_string(),
                        );
                        return;
                    }
                    match fetch_internal_notes(&token, &ticket_context, &ticket_id).await {
                        Ok(list) => set_notes.set(list),
                        Err(e) => set_status.set(format!("couldn't load notes: {e}")),
                    }
                });
            }
        }
    };
    let add_note = {
        let ticket_id = ticket_id.clone();
        let my_sub = my_sub.clone();
        let token = token.clone();
        let ticket_context = ticket_context.clone();
        move |_| {
            let text = note_text.get();
            if text.trim().is_empty() {
                return;
            }
            if ticket_context == TicketContext::Resolving {
                set_status.set(
                    "still working out which context to use - try again in a moment".to_string(),
                );
                return;
            }
            set_note_text.set(String::new());
            let ticket_id = ticket_id.clone();
            let staff_id = my_sub.clone();
            let token = token.clone();
            let ticket_context = ticket_context.clone();
            spawn_local(async move {
                let payload = serde_json::json!({ "ticket_id": ticket_id, "staff_id": staff_id, "note": text });
                match api::submit_command(
                    &token,
                    ticket_context.bounded_context(),
                    "AddInternalNote",
                    &payload,
                )
                .await
                {
                    // The re-fetch uses the same context as the write that
                    // preceded it, so a note can never be written to one
                    // context and read back from another.
                    Ok(_) => {
                        match fetch_internal_notes(&token, &ticket_context, &ticket_id).await {
                            Ok(list) => set_notes.set(list),
                            Err(e) => set_status.set(format!("couldn't reload notes: {e}")),
                        }
                    }
                    Err(e) => set_status.set(format!("AddInternalNote failed: {e}")),
                }
            });
        }
    };

    // Merge - staff-only, on either side (this ticket becomes the
    // primary, whatever's typed in becomes the duplicate) - no
    // ticket-picker UI, just a raw id, same "type the id in" shortcut
    // `on_create` above already takes for its own generated ticket_id.
    let (merge_duplicate_id, set_merge_duplicate_id) = signal(String::new());
    let can_merge = is_staff && !matches!(ticket.status.as_str(), "closed" | "merged");
    let merge = {
        let ticket_id = ticket_id.clone();
        let run = run.clone();
        move |_| {
            let duplicate_ticket_id = merge_duplicate_id.get();
            if duplicate_ticket_id.trim().is_empty() {
                return;
            }
            set_merge_duplicate_id.set(String::new());
            run(
                "MergeTickets",
                serde_json::json!({ "primary_ticket_id": ticket_id, "duplicate_ticket_id": duplicate_ticket_id }),
            )
        }
    };

    let status_badge_class = format!("badge status-{}", ticket.status);
    let status_text = ticket.status.replace('_', " ");
    let priority_badge_class = format!("badge priority-{}", ticket.priority);
    let priority_text = ticket.priority.clone();
    let escalated = ticket.escalated;
    let status_for_staff_1 = ticket.status.clone();
    let status_for_staff_2 = ticket.status.clone();
    let status_for_staff_3 = ticket.status.clone();
    let status_for_customer_1 = ticket.status.clone();
    let can_ask = is_staff && ticket.status == "in_progress";
    let can_reply = !is_staff && is_own_ticket && ticket.status == "waiting_on_customer";
    let (title, description, messages, given_comment) = match content {
        Some(c) => (c.title, c.description, c.messages, c.rating_comment),
        None => ("…".to_string(), String::new(), Vec::new(), None),
    };

    view! {
        <tr>
            <td>{title}</td>
            <td><span class=status_badge_class>{status_text}</span></td>
            <td>
                <span class=priority_badge_class>{priority_text}</span>
                {escalated.then(|| view! { <span class="badge escalated">"⚠ Escalated"</span> })}
            </td>
            <td>{short_id(&ticket.requester_id)}</td>
            <td>{ticket.assigned_staff_id.as_deref().map(short_id).unwrap_or_default()}</td>
            <td>
                {(is_staff && status_for_staff_1 == "open").then(|| view! { <button on:click=assign>"Assign to me"</button> })}
                {(is_staff && status_for_staff_2 == "in_progress").then(|| view! { <button on:click=resolve>"Resolve"</button> })}
                {(is_staff && status_for_staff_3 == "resolved").then(|| view! { <button on:click=close>"Close"</button> })}
                {(!is_staff && status_for_customer_1 == "resolved").then(|| view! { <button on:click=reopen>"Reopen"</button> })}
            </td>
        </tr>
        <tr>
            <td colspan="6" class="ticket-detail">
                <p class="description">{description}</p>
                {(!messages.is_empty()).then(|| view! {
                    <ul class="messages">
                        {messages.into_iter().map(|m| {
                            let who = if m.from_staff { "Staff" } else { "Customer" };
                            view! {
                                <li class=if m.from_staff { "from-staff" } else { "from-customer" }>
                                    <strong>{format!("{who} ({}): ", short_id(&m.author_id))}</strong>
                                    {m.text}
                                </li>
                            }
                        }).collect_view()}
                    </ul>
                })}
                {can_ask.then(|| view! {
                    <div class="reply-form">
                        <input
                            type="text"
                            placeholder="Ask the customer something..."
                            prop:value=message_text
                            on:input:target=move |ev| set_message_text.set(ev.target().value())
                        />
                        <button on:click=ask>"Request info"</button>
                    </div>
                })}
                {can_reply.then(|| view! {
                    <div class="reply-form">
                        <input
                            type="text"
                            placeholder="Your reply..."
                            prop:value=message_text
                            on:input:target=move |ev| set_message_text.set(ev.target().value())
                        />
                        <button on:click=reply>"Reply"</button>
                    </div>
                })}
                {existing_rating.map(|r| view! {
                    <p class="rating-given">
                        {format!("Rated {r}★")}
                        {given_comment.clone().map(|c| format!(" — {c}"))}
                    </p>
                })}
                {can_rate.then(|| view! {
                    <div class="reply-form">
                        <select on:change:target=move |ev| set_rating_value.set(ev.target().value().parse().unwrap_or(5))>
                            <option value="5">"★★★★★ (5)"</option>
                            <option value="4">"★★★★ (4)"</option>
                            <option value="3">"★★★ (3)"</option>
                            <option value="2">"★★ (2)"</option>
                            <option value="1">"★ (1)"</option>
                        </select>
                        <input
                            type="text"
                            placeholder="Comment (optional)"
                            prop:value=rating_comment
                            on:input:target=move |ev| set_rating_comment.set(ev.target().value())
                        />
                        <button on:click=submit_rating>"Rate"</button>
                    </div>
                })}
                {is_staff.then(|| view! {
                    <div class="internal-notes">
                        <button on:click=toggle_notes>
                            {move || if notes_open.get() { "Hide notes" } else { "Notes" }}
                        </button>
                        {move || {
                            let add_note = add_note.clone();
                            notes_open.get().then(move || view! {
                                <div>
                                    <ul class="messages">
                                        {move || notes.get().into_iter().map(|n| view! {
                                            <li class="from-staff">
                                                <strong>{format!("{}: ", short_id(&n.staff_id))}</strong>
                                                {n.note}
                                            </li>
                                        }).collect_view()}
                                    </ul>
                                    <div class="reply-form">
                                        <input
                                            type="text"
                                            placeholder="Leave a note for other staff..."
                                            prop:value=note_text
                                            on:input:target=move |ev| set_note_text.set(ev.target().value())
                                        />
                                        <button on:click=add_note>"Add note"</button>
                                    </div>
                                </div>
                            })
                        }}
                    </div>
                })}
                {can_merge.then(|| view! {
                    <div class="reply-form">
                        <input
                            type="text"
                            placeholder="Duplicate ticket id to merge into this one..."
                            prop:value=merge_duplicate_id
                            on:input:target=move |ev| set_merge_duplicate_id.set(ev.target().value())
                        />
                        <button on:click=merge>"Merge"</button>
                    </div>
                })}
            </td>
        </tr>
    }
}
