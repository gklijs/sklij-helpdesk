//! Company signup, tenant provisioning, trial lifecycle and lifecycle
//! mirroring. `SignUpCompany`'s field validation (email shape, empty
//! ids/names) is already covered by `helpdesk.rs`'s own
//! `sign_up_validation_tests`; only what that module leaves out is here.

use serde_json::json;
use skilj::ScheduleDeadline;
use skilj_core::shared::Tag;
use skilj_helpdesk::helpdesk::*;
use skilj_helpdesk::scheduling;
use skilj_test_fixture::command::GivenEvents;

use crate::events::*;

// --- SignUpCompany ---

#[test]
fn sign_up_records_the_company() {
    GivenEvents::<SignUpCompany>::new()
        .when(SignUpCompanyPayload {
            company_id: "acme".into(),
            name: "Acme".into(),
            contact_email: "ops@acme.example".into(),
        })
        .then_accepted(vec![spec(
            "CompanySignedUp",
            json!({ "company_id": "acme", "name": "Acme", "contact_email": "ops@acme.example" }),
        )]);
}

#[test]
fn signing_up_twice_is_rejected_whatever_the_current_status() {
    for history in [
        vec![signed_up("acme")],
        vec![signed_up("acme"), activated("acme")],
        vec![signed_up("acme"), expired("acme")],
    ] {
        GivenEvents::<SignUpCompany>::new()
            .events(history)
            .when(SignUpCompanyPayload {
                company_id: "acme".into(),
                name: "Acme again".into(),
                contact_email: "ops@acme.example".into(),
            })
            .then_rejected("already_signed_up");
    }
}

#[test]
fn over_long_company_ids_and_names_are_rejected() {
    let long = "x".repeat(257);
    GivenEvents::<SignUpCompany>::new()
        .when(SignUpCompanyPayload {
            company_id: long.clone(),
            name: "Acme".into(),
            contact_email: "ops@acme.example".into(),
        })
        .then_rejected("company_id_too_long");
    GivenEvents::<SignUpCompany>::new()
        .when(SignUpCompanyPayload {
            company_id: "acme".into(),
            name: long,
            contact_email: "ops@acme.example".into(),
        })
        .then_rejected("company_name_too_long");
}

// --- RecordCompanyTenant ---

#[test]
fn a_signed_up_company_gets_its_tenant_recorded() {
    GivenEvents::<RecordCompanyTenant>::new()
        .event(signed_up("acme"))
        .when(RecordCompanyTenantPayload {
            company_id: "acme".into(),
            tenant_name: "tenant-acme".into(),
        })
        .then_accepted(vec![spec(
            "CompanyTenantProvisioned",
            json!({ "company_id": "acme", "tenant_name": "tenant-acme" }),
        )]);
}

#[test]
fn a_tenant_for_an_unknown_company_is_rejected() {
    GivenEvents::<RecordCompanyTenant>::new()
        .when(RecordCompanyTenantPayload {
            company_id: "acme".into(),
            tenant_name: "tenant-acme".into(),
        })
        .then_rejected("company_not_found");
}

#[test]
fn a_second_tenant_for_the_same_company_is_rejected() {
    GivenEvents::<RecordCompanyTenant>::new()
        .event(signed_up("acme"))
        .event(tenant_provisioned("acme", "tenant-acme"))
        .when(RecordCompanyTenantPayload {
            company_id: "acme".into(),
            tenant_name: "tenant-acme-2".into(),
        })
        .then_rejected("tenant_already_provisioned");
}

// --- ConvertCompanyTrial / ExpireCompanyTrial / ReactivateCompany ---

#[test]
fn a_trialing_company_converts_or_expires() {
    GivenEvents::<ConvertCompanyTrial>::new()
        .event(signed_up("acme"))
        .when(ConvertCompanyTrialPayload {
            company_id: "acme".into(),
        })
        .then_accepted(vec![spec(
            "CompanyActivated",
            json!({ "company_id": "acme" }),
        )]);
    GivenEvents::<ExpireCompanyTrial>::new()
        .event(signed_up("acme"))
        .when(ExpireCompanyTrialPayload {
            company_id: "acme".into(),
        })
        .then_accepted(vec![spec(
            "CompanyExpired",
            json!({ "company_id": "acme" }),
        )]);
}

#[test]
fn trial_deadlines_for_an_unknown_company_are_rejected() {
    GivenEvents::<ConvertCompanyTrial>::new()
        .when(ConvertCompanyTrialPayload {
            company_id: "acme".into(),
        })
        .then_rejected("company_not_found");
    GivenEvents::<ExpireCompanyTrial>::new()
        .when(ExpireCompanyTrialPayload {
            company_id: "acme".into(),
        })
        .then_rejected("company_not_found");
}

#[test]
fn a_trial_that_already_ended_neither_converts_nor_expires_again() {
    for history in [
        vec![signed_up("acme"), activated("acme")],
        vec![signed_up("acme"), expired("acme")],
    ] {
        GivenEvents::<ConvertCompanyTrial>::new()
            .events(history)
            .when(ConvertCompanyTrialPayload {
                company_id: "acme".into(),
            })
            .then_rejected("company_not_trialing");
    }
    for history in [
        vec![signed_up("acme"), activated("acme")],
        vec![signed_up("acme"), expired("acme")],
    ] {
        GivenEvents::<ExpireCompanyTrial>::new()
            .events(history)
            .when(ExpireCompanyTrialPayload {
                company_id: "acme".into(),
            })
            .then_rejected("company_not_trialing");
    }
}

#[test]
fn only_an_expired_company_can_be_reactivated() {
    GivenEvents::<ReactivateCompany>::new()
        .events([signed_up("acme"), expired("acme")])
        .when(ReactivateCompanyPayload {
            company_id: "acme".into(),
        })
        .then_accepted(vec![spec(
            "CompanyActivated",
            json!({ "company_id": "acme" }),
        )]);
    for history in [
        vec![signed_up("acme")],
        vec![signed_up("acme"), activated("acme")],
        vec![signed_up("acme"), expired("acme"), activated("acme")],
    ] {
        GivenEvents::<ReactivateCompany>::new()
            .events(history)
            .when(ReactivateCompanyPayload {
                company_id: "acme".into(),
            })
            .then_rejected("company_not_expired");
    }
    GivenEvents::<ReactivateCompany>::new()
        .when(ReactivateCompanyPayload {
            company_id: "acme".into(),
        })
        .then_rejected("company_not_found");
}

#[test]
fn another_companys_history_does_not_count() {
    GivenEvents::<ConvertCompanyTrial>::new()
        .events([signed_up("other"), expired("other")])
        .when(ConvertCompanyTrialPayload {
            company_id: "acme".into(),
        })
        .then_rejected("company_not_found");
}

// --- RecordTenantLifecycle ---

fn mirror(status: CompanyStatus) -> RecordTenantLifecyclePayload {
    RecordTenantLifecyclePayload {
        company_id: "acme".into(),
        status,
        source_event_type: "CompanySignedUp".into(),
    }
}

#[test]
fn the_first_mirror_into_an_empty_tenant_is_accepted() {
    // A tenant's own history starts empty - there's no CompanySignedUp
    // there to be "not found" against.
    GivenEvents::<RecordTenantLifecycle>::new()
        .when(mirror(CompanyStatus::Trialing))
        .then_accepted(vec![spec(
            "CompanyLifecycleMirrored",
            json!({ "company_id": "acme", "status": "trialing", "source_event_type": "CompanySignedUp" }),
        )]);
}

#[test]
fn mirroring_the_current_status_again_is_rejected() {
    GivenEvents::<RecordTenantLifecycle>::new()
        .event(mirrored("acme", CompanyStatus::Trialing))
        .when(mirror(CompanyStatus::Trialing))
        .then_rejected("lifecycle_already_mirrored");
}

#[test]
fn mirroring_moves_forward_from_trialing_through_expired_to_active() {
    GivenEvents::<RecordTenantLifecycle>::new()
        .event(mirrored("acme", CompanyStatus::Trialing))
        .when(mirror(CompanyStatus::Expired))
        .then(assert_accepted);
    GivenEvents::<RecordTenantLifecycle>::new()
        .events([
            mirrored("acme", CompanyStatus::Trialing),
            mirrored("acme", CompanyStatus::Expired),
        ])
        .when(mirror(CompanyStatus::Active))
        .then(assert_accepted);
}

#[test]
fn a_late_mirror_never_moves_a_tenant_backwards() {
    for (current, late) in [
        (CompanyStatus::Active, CompanyStatus::Trialing),
        (CompanyStatus::Active, CompanyStatus::Expired),
        (CompanyStatus::Expired, CompanyStatus::Trialing),
    ] {
        GivenEvents::<RecordTenantLifecycle>::new()
            .event(mirrored("acme", current))
            .when(mirror(late))
            .then_rejected("lifecycle_regression");
    }
}

#[test]
fn company_status_folds_lifecycle_events_and_mirrors_by_recency() {
    // The shared context's own CompanyActivated and a mirror carrying the
    // same status are one status, not two.
    GivenEvents::<RecordTenantLifecycle>::new()
        .events([signed_up("acme"), activated("acme")])
        .when(mirror(CompanyStatus::Active))
        .then_rejected("lifecycle_already_mirrored");
}

// --- trial deadlines ---

#[test]
fn signup_schedules_a_trial_conversion_for_the_company() {
    let source = CompanySignedUpPayload {
        company_id: "acme".into(),
        name: "Acme".into(),
        contact_email: "ops@acme.example".into(),
    };
    let before = chrono::Utc::now();
    let deadline = ScheduleCompanyTrialConversion::schedule(&source)
        .expect("the mocked charge always succeeds");
    let after = chrono::Utc::now();
    assert_eq!(deadline.payload.company_id, "acme");
    assert_eq!(
        deadline.tags,
        vec![Tag {
            key: "company".into(),
            value: Some("acme".into())
        }]
    );
    let trial = scheduling::trial_duration();
    assert!(deadline.fire_at >= before + trial && deadline.fire_at <= after + trial);
    // Expiry is the other branch of the same mocked charge - never both.
    assert!(ScheduleCompanyTrialExpiry::schedule(&source).is_none());
}
