//! Go en Rust lezen en schrijven dezelfde nulwaarden en gevulde objecten.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use spin_domain::{Wire, json};
fn check<T: Wire>(input: &[u8]) {
    let before = json::parse(input).unwrap();
    let output = T::from_json(input).unwrap().to_json().unwrap();
    let after = json::parse(output.as_bytes()).unwrap();
    assert_eq!(before, after);
}
#[test]
fn workflowaction_zero() {
    check::<spin_domain::WorkflowAction>(include_bytes!("fixtures/WorkflowAction-zero.json"));
}
#[test]
fn workflowaction_filled() {
    check::<spin_domain::WorkflowAction>(include_bytes!("fixtures/WorkflowAction-filled.json"));
}
#[test]
fn workflowtransition_zero() {
    check::<spin_domain::WorkflowTransition>(include_bytes!(
        "fixtures/WorkflowTransition-zero.json"
    ));
}
#[test]
fn workflowtransition_filled() {
    check::<spin_domain::WorkflowTransition>(include_bytes!(
        "fixtures/WorkflowTransition-filled.json"
    ));
}
#[test]
fn deliverabledefinition_zero() {
    check::<spin_domain::DeliverableDefinition>(include_bytes!(
        "fixtures/DeliverableDefinition-zero.json"
    ));
}
#[test]
fn deliverabledefinition_filled() {
    check::<spin_domain::DeliverableDefinition>(include_bytes!(
        "fixtures/DeliverableDefinition-filled.json"
    ));
}
#[test]
fn deliverablebundle_zero() {
    check::<spin_domain::DeliverableBundle>(include_bytes!("fixtures/DeliverableBundle-zero.json"));
}
#[test]
fn deliverablebundle_filled() {
    check::<spin_domain::DeliverableBundle>(include_bytes!(
        "fixtures/DeliverableBundle-filled.json"
    ));
}
#[test]
fn workflowphase_zero() {
    check::<spin_domain::WorkflowPhase>(include_bytes!("fixtures/WorkflowPhase-zero.json"));
}
#[test]
fn workflowphase_filled() {
    check::<spin_domain::WorkflowPhase>(include_bytes!("fixtures/WorkflowPhase-filled.json"));
}
#[test]
fn workflowtemplate_zero() {
    check::<spin_domain::WorkflowTemplate>(include_bytes!("fixtures/WorkflowTemplate-zero.json"));
}
#[test]
fn workflowtemplate_filled() {
    check::<spin_domain::WorkflowTemplate>(include_bytes!("fixtures/WorkflowTemplate-filled.json"));
}
#[test]
fn phaserun_zero() {
    check::<spin_domain::PhaseRun>(include_bytes!("fixtures/PhaseRun-zero.json"));
}
#[test]
fn phaserun_filled() {
    check::<spin_domain::PhaseRun>(include_bytes!("fixtures/PhaseRun-filled.json"));
}
#[test]
fn chatline_zero() {
    check::<spin_domain::ChatLine>(include_bytes!("fixtures/ChatLine-zero.json"));
}
#[test]
fn chatline_filled() {
    check::<spin_domain::ChatLine>(include_bytes!("fixtures/ChatLine-filled.json"));
}
#[test]
fn workflowactionresult_zero() {
    check::<spin_domain::WorkflowActionResult>(include_bytes!(
        "fixtures/WorkflowActionResult-zero.json"
    ));
}
#[test]
fn workflowactionresult_filled() {
    check::<spin_domain::WorkflowActionResult>(include_bytes!(
        "fixtures/WorkflowActionResult-filled.json"
    ));
}
#[test]
fn workflowagentoutcome_zero() {
    check::<spin_domain::WorkflowAgentOutcome>(include_bytes!(
        "fixtures/WorkflowAgentOutcome-zero.json"
    ));
}
#[test]
fn workflowagentoutcome_filled() {
    check::<spin_domain::WorkflowAgentOutcome>(include_bytes!(
        "fixtures/WorkflowAgentOutcome-filled.json"
    ));
}
#[test]
fn deliverable_zero() {
    check::<spin_domain::Deliverable>(include_bytes!("fixtures/Deliverable-zero.json"));
}
#[test]
fn deliverable_filled() {
    check::<spin_domain::Deliverable>(include_bytes!("fixtures/Deliverable-filled.json"));
}
#[test]
fn deliverablecomment_zero() {
    check::<spin_domain::DeliverableComment>(include_bytes!(
        "fixtures/DeliverableComment-zero.json"
    ));
}
#[test]
fn deliverablecomment_filled() {
    check::<spin_domain::DeliverableComment>(include_bytes!(
        "fixtures/DeliverableComment-filled.json"
    ));
}
#[test]
fn codereviewrevision_zero() {
    check::<spin_domain::CodeReviewRevision>(include_bytes!(
        "fixtures/CodeReviewRevision-zero.json"
    ));
}
#[test]
fn codereviewrevision_filled() {
    check::<spin_domain::CodeReviewRevision>(include_bytes!(
        "fixtures/CodeReviewRevision-filled.json"
    ));
}
#[test]
fn codereviewfile_zero() {
    check::<spin_domain::CodeReviewFile>(include_bytes!("fixtures/CodeReviewFile-zero.json"));
}
#[test]
fn codereviewfile_filled() {
    check::<spin_domain::CodeReviewFile>(include_bytes!("fixtures/CodeReviewFile-filled.json"));
}
#[test]
fn codereviewrevisionsummary_zero() {
    check::<spin_domain::CodeReviewRevisionSummary>(include_bytes!(
        "fixtures/CodeReviewRevisionSummary-zero.json"
    ));
}
#[test]
fn codereviewrevisionsummary_filled() {
    check::<spin_domain::CodeReviewRevisionSummary>(include_bytes!(
        "fixtures/CodeReviewRevisionSummary-filled.json"
    ));
}
#[test]
fn codereviewcomment_zero() {
    check::<spin_domain::CodeReviewComment>(include_bytes!("fixtures/CodeReviewComment-zero.json"));
}
#[test]
fn codereviewcomment_filled() {
    check::<spin_domain::CodeReviewComment>(include_bytes!(
        "fixtures/CodeReviewComment-filled.json"
    ));
}
#[test]
fn workflowquestionitem_zero() {
    check::<spin_domain::WorkflowQuestionItem>(include_bytes!(
        "fixtures/WorkflowQuestionItem-zero.json"
    ));
}
#[test]
fn workflowquestionitem_filled() {
    check::<spin_domain::WorkflowQuestionItem>(include_bytes!(
        "fixtures/WorkflowQuestionItem-filled.json"
    ));
}
#[test]
fn workflowquestionanswer_zero() {
    check::<spin_domain::WorkflowQuestionAnswer>(include_bytes!(
        "fixtures/WorkflowQuestionAnswer-zero.json"
    ));
}
#[test]
fn workflowquestionanswer_filled() {
    check::<spin_domain::WorkflowQuestionAnswer>(include_bytes!(
        "fixtures/WorkflowQuestionAnswer-filled.json"
    ));
}
#[test]
fn workflowquestion_zero() {
    check::<spin_domain::WorkflowQuestion>(include_bytes!("fixtures/WorkflowQuestion-zero.json"));
}
#[test]
fn workflowquestion_filled() {
    check::<spin_domain::WorkflowQuestion>(include_bytes!("fixtures/WorkflowQuestion-filled.json"));
}
#[test]
fn layercontents_zero() {
    check::<spin_domain::LayerContents>(include_bytes!("fixtures/LayerContents-zero.json"));
}
#[test]
fn layercontents_filled() {
    check::<spin_domain::LayerContents>(include_bytes!("fixtures/LayerContents-filled.json"));
}
#[test]
fn contententry_zero() {
    check::<spin_domain::ContentEntry>(include_bytes!("fixtures/ContentEntry-zero.json"));
}
#[test]
fn contententry_filled() {
    check::<spin_domain::ContentEntry>(include_bytes!("fixtures/ContentEntry-filled.json"));
}
#[test]
fn contenttotal_zero() {
    check::<spin_domain::ContentTotal>(include_bytes!("fixtures/ContentTotal-zero.json"));
}
#[test]
fn contenttotal_filled() {
    check::<spin_domain::ContentTotal>(include_bytes!("fixtures/ContentTotal-filled.json"));
}
#[test]
fn capsulesnapshot_zero() {
    check::<spin_domain::CapsuleSnapshot>(include_bytes!("fixtures/CapsuleSnapshot-zero.json"));
}
#[test]
fn capsulesnapshot_filled() {
    check::<spin_domain::CapsuleSnapshot>(include_bytes!("fixtures/CapsuleSnapshot-filled.json"));
}
#[test]
fn capsuleruntime_zero() {
    check::<spin_domain::CapsuleRuntime>(include_bytes!("fixtures/CapsuleRuntime-zero.json"));
}
#[test]
fn capsuleruntime_filled() {
    check::<spin_domain::CapsuleRuntime>(include_bytes!("fixtures/CapsuleRuntime-filled.json"));
}
#[test]
fn capsuleengineinfo_zero() {
    check::<spin_domain::CapsuleEngineInfo>(include_bytes!("fixtures/CapsuleEngineInfo-zero.json"));
}
#[test]
fn capsuleengineinfo_filled() {
    check::<spin_domain::CapsuleEngineInfo>(include_bytes!(
        "fixtures/CapsuleEngineInfo-filled.json"
    ));
}
#[test]
fn enablement_zero() {
    check::<spin_domain::Enablement>(include_bytes!("fixtures/Enablement-zero.json"));
}
#[test]
fn enablement_filled() {
    check::<spin_domain::Enablement>(include_bytes!("fixtures/Enablement-filled.json"));
}
#[test]
fn agentoption_zero() {
    check::<spin_domain::AgentOption>(include_bytes!("fixtures/AgentOption-zero.json"));
}
#[test]
fn agentoption_filled() {
    check::<spin_domain::AgentOption>(include_bytes!("fixtures/AgentOption-filled.json"));
}
#[test]
fn login_zero() {
    check::<spin_domain::Login>(include_bytes!("fixtures/Login-zero.json"));
}
#[test]
fn login_filled() {
    check::<spin_domain::Login>(include_bytes!("fixtures/Login-filled.json"));
}
#[test]
fn loginfile_zero() {
    check::<spin_domain::LoginFile>(include_bytes!("fixtures/LoginFile-zero.json"));
}
#[test]
fn loginfile_filled() {
    check::<spin_domain::LoginFile>(include_bytes!("fixtures/LoginFile-filled.json"));
}
#[test]
fn loginsummary_zero() {
    check::<spin_domain::LoginSummary>(include_bytes!("fixtures/LoginSummary-zero.json"));
}
#[test]
fn loginsummary_filled() {
    check::<spin_domain::LoginSummary>(include_bytes!("fixtures/LoginSummary-filled.json"));
}
#[test]
fn agentsettings_zero() {
    check::<spin_domain::AgentSettings>(include_bytes!("fixtures/AgentSettings-zero.json"));
}
#[test]
fn agentsettings_filled() {
    check::<spin_domain::AgentSettings>(include_bytes!("fixtures/AgentSettings-filled.json"));
}
#[test]
fn agentoptions_zero() {
    check::<spin_domain::AgentOptions>(include_bytes!("fixtures/AgentOptions-zero.json"));
}
#[test]
fn agentoptions_filled() {
    check::<spin_domain::AgentOptions>(include_bytes!("fixtures/AgentOptions-filled.json"));
}
#[test]
fn artifact_zero() {
    check::<spin_domain::Artifact>(include_bytes!("fixtures/Artifact-zero.json"));
}
#[test]
fn artifact_filled() {
    check::<spin_domain::Artifact>(include_bytes!("fixtures/Artifact-filled.json"));
}
#[test]
fn recordingcommand_zero() {
    check::<spin_domain::RecordingCommand>(include_bytes!("fixtures/RecordingCommand-zero.json"));
}
#[test]
fn recordingcommand_filled() {
    check::<spin_domain::RecordingCommand>(include_bytes!("fixtures/RecordingCommand-filled.json"));
}
#[test]
fn recording_zero() {
    check::<spin_domain::Recording>(include_bytes!("fixtures/Recording-zero.json"));
}
#[test]
fn recording_filled() {
    check::<spin_domain::Recording>(include_bytes!("fixtures/Recording-filled.json"));
}
#[test]
fn resolvedartifact_zero() {
    check::<spin_domain::ResolvedArtifact>(include_bytes!("fixtures/ResolvedArtifact-zero.json"));
}
#[test]
fn resolvedartifact_filled() {
    check::<spin_domain::ResolvedArtifact>(include_bytes!("fixtures/ResolvedArtifact-filled.json"));
}
#[test]
fn jobrepository_zero() {
    check::<spin_domain::JobRepository>(include_bytes!("fixtures/JobRepository-zero.json"));
}
#[test]
fn jobrepository_filled() {
    check::<spin_domain::JobRepository>(include_bytes!("fixtures/JobRepository-filled.json"));
}
#[test]
fn jobrepositoryrequest_zero() {
    check::<spin_domain::JobRepositoryRequest>(include_bytes!(
        "fixtures/JobRepositoryRequest-zero.json"
    ));
}
#[test]
fn jobrepositoryrequest_filled() {
    check::<spin_domain::JobRepositoryRequest>(include_bytes!(
        "fixtures/JobRepositoryRequest-filled.json"
    ));
}
#[test]
fn gitworkspace_zero() {
    check::<spin_domain::GitWorkspace>(include_bytes!("fixtures/GitWorkspace-zero.json"));
}
#[test]
fn gitworkspace_filled() {
    check::<spin_domain::GitWorkspace>(include_bytes!("fixtures/GitWorkspace-filled.json"));
}
#[test]
fn composition_zero() {
    check::<spin_domain::Composition>(include_bytes!("fixtures/Composition-zero.json"));
}
#[test]
fn composition_filled() {
    check::<spin_domain::Composition>(include_bytes!("fixtures/Composition-filled.json"));
}
#[test]
fn agentprocess_zero() {
    check::<spin_domain::AgentProcess>(include_bytes!("fixtures/AgentProcess-zero.json"));
}
#[test]
fn agentprocess_filled() {
    check::<spin_domain::AgentProcess>(include_bytes!("fixtures/AgentProcess-filled.json"));
}
#[test]
fn job_zero() {
    check::<spin_domain::Job>(include_bytes!("fixtures/Job-zero.json"));
}
#[test]
fn job_filled() {
    check::<spin_domain::Job>(include_bytes!("fixtures/Job-filled.json"));
}
#[test]
fn jobattachment_zero() {
    check::<spin_domain::JobAttachment>(include_bytes!("fixtures/JobAttachment-zero.json"));
}
#[test]
fn jobattachment_filled() {
    check::<spin_domain::JobAttachment>(include_bytes!("fixtures/JobAttachment-filled.json"));
}
#[test]
fn session_zero() {
    check::<spin_domain::Session>(include_bytes!("fixtures/Session-zero.json"));
}
#[test]
fn session_filled() {
    check::<spin_domain::Session>(include_bytes!("fixtures/Session-filled.json"));
}
#[test]
fn turn_zero() {
    check::<spin_domain::Turn>(include_bytes!("fixtures/Turn-zero.json"));
}
#[test]
fn turn_filled() {
    check::<spin_domain::Turn>(include_bytes!("fixtures/Turn-filled.json"));
}
#[test]
fn activation_zero() {
    check::<spin_domain::Activation>(include_bytes!("fixtures/Activation-zero.json"));
}
#[test]
fn activation_filled() {
    check::<spin_domain::Activation>(include_bytes!("fixtures/Activation-filled.json"));
}
#[test]
fn capsulemanifest_zero() {
    check::<spin_domain::CapsuleManifest>(include_bytes!("fixtures/CapsuleManifest-zero.json"));
}
#[test]
fn capsulemanifest_filled() {
    check::<spin_domain::CapsuleManifest>(include_bytes!("fixtures/CapsuleManifest-filled.json"));
}
#[test]
fn checkpoint_zero() {
    check::<spin_domain::Checkpoint>(include_bytes!("fixtures/Checkpoint-zero.json"));
}
#[test]
fn checkpoint_filled() {
    check::<spin_domain::Checkpoint>(include_bytes!("fixtures/Checkpoint-filled.json"));
}
#[test]
fn testevidence_zero() {
    check::<spin_domain::TestEvidence>(include_bytes!("fixtures/TestEvidence-zero.json"));
}
#[test]
fn testevidence_filled() {
    check::<spin_domain::TestEvidence>(include_bytes!("fixtures/TestEvidence-filled.json"));
}
#[test]
fn criterionevidence_zero() {
    check::<spin_domain::CriterionEvidence>(include_bytes!("fixtures/CriterionEvidence-zero.json"));
}
#[test]
fn criterionevidence_filled() {
    check::<spin_domain::CriterionEvidence>(include_bytes!(
        "fixtures/CriterionEvidence-filled.json"
    ));
}
#[test]
fn usage_zero() {
    check::<spin_domain::Usage>(include_bytes!("fixtures/Usage-zero.json"));
}
#[test]
fn usage_filled() {
    check::<spin_domain::Usage>(include_bytes!("fixtures/Usage-filled.json"));
}
#[test]
fn result_zero() {
    check::<spin_domain::Result>(include_bytes!("fixtures/Result-zero.json"));
}
#[test]
fn result_filled() {
    check::<spin_domain::Result>(include_bytes!("fixtures/Result-filled.json"));
}
#[test]
fn clientcapabilities_zero() {
    check::<spin_domain::ClientCapabilities>(include_bytes!(
        "fixtures/ClientCapabilities-zero.json"
    ));
}
#[test]
fn clientcapabilities_filled() {
    check::<spin_domain::ClientCapabilities>(include_bytes!(
        "fixtures/ClientCapabilities-filled.json"
    ));
}
#[test]
fn client_zero() {
    check::<spin_domain::Client>(include_bytes!("fixtures/Client-zero.json"));
}
#[test]
fn client_filled() {
    check::<spin_domain::Client>(include_bytes!("fixtures/Client-filled.json"));
}
#[test]
fn mcpsecret_zero() {
    check::<spin_domain::MCPSecret>(include_bytes!("fixtures/MCPSecret-zero.json"));
}
#[test]
fn mcpsecret_filled() {
    check::<spin_domain::MCPSecret>(include_bytes!("fixtures/MCPSecret-filled.json"));
}
#[test]
fn mcpserver_zero() {
    check::<spin_domain::MCPServer>(include_bytes!("fixtures/MCPServer-zero.json"));
}
#[test]
fn mcpserver_filled() {
    check::<spin_domain::MCPServer>(include_bytes!("fixtures/MCPServer-filled.json"));
}
#[test]
fn gitrepository_zero() {
    check::<spin_domain::GitRepository>(include_bytes!("fixtures/GitRepository-zero.json"));
}
#[test]
fn gitrepository_filled() {
    check::<spin_domain::GitRepository>(include_bytes!("fixtures/GitRepository-filled.json"));
}
#[test]
fn appservice_zero() {
    check::<spin_domain::AppService>(include_bytes!("fixtures/AppService-zero.json"));
}
#[test]
fn appservice_filled() {
    check::<spin_domain::AppService>(include_bytes!("fixtures/AppService-filled.json"));
}
#[test]
fn appserviceruntime_zero() {
    check::<spin_domain::AppServiceRuntime>(include_bytes!("fixtures/AppServiceRuntime-zero.json"));
}
#[test]
fn appserviceruntime_filled() {
    check::<spin_domain::AppServiceRuntime>(include_bytes!(
        "fixtures/AppServiceRuntime-filled.json"
    ));
}
#[test]
fn gitaccount_zero() {
    check::<spin_domain::GitAccount>(include_bytes!("fixtures/GitAccount-zero.json"));
}
#[test]
fn gitaccount_filled() {
    check::<spin_domain::GitAccount>(include_bytes!("fixtures/GitAccount-filled.json"));
}
#[test]
fn user_zero() {
    check::<spin_domain::User>(include_bytes!("fixtures/User-zero.json"));
}
#[test]
fn user_filled() {
    check::<spin_domain::User>(include_bytes!("fixtures/User-filled.json"));
}
#[test]
fn publicuser_zero() {
    check::<spin_domain::PublicUser>(include_bytes!("fixtures/PublicUser-zero.json"));
}
#[test]
fn publicuser_filled() {
    check::<spin_domain::PublicUser>(include_bytes!("fixtures/PublicUser-filled.json"));
}
#[test]
fn authsession_zero() {
    check::<spin_domain::AuthSession>(include_bytes!("fixtures/AuthSession-zero.json"));
}
#[test]
fn authsession_filled() {
    check::<spin_domain::AuthSession>(include_bytes!("fixtures/AuthSession-filled.json"));
}
#[test]
fn gitoauthconfiguration_zero() {
    check::<spin_domain::GitOAuthConfiguration>(include_bytes!(
        "fixtures/GitOAuthConfiguration-zero.json"
    ));
}
#[test]
fn gitoauthconfiguration_filled() {
    check::<spin_domain::GitOAuthConfiguration>(include_bytes!(
        "fixtures/GitOAuthConfiguration-filled.json"
    ));
}
#[test]
fn snapshot_zero() {
    check::<spin_domain::Snapshot>(include_bytes!("fixtures/Snapshot-zero.json"));
}
#[test]
fn snapshot_filled() {
    check::<spin_domain::Snapshot>(include_bytes!("fixtures/Snapshot-filled.json"));
}
#[test]
fn recommendation_zero() {
    check::<spin_domain::Recommendation>(include_bytes!("fixtures/Recommendation-zero.json"));
}
#[test]
fn recommendation_filled() {
    check::<spin_domain::Recommendation>(include_bytes!("fixtures/Recommendation-filled.json"));
}
#[test]
fn createrecordingrequest_zero() {
    check::<spin_domain::CreateRecordingRequest>(include_bytes!(
        "fixtures/CreateRecordingRequest-zero.json"
    ));
}
#[test]
fn createrecordingrequest_filled() {
    check::<spin_domain::CreateRecordingRequest>(include_bytes!(
        "fixtures/CreateRecordingRequest-filled.json"
    ));
}
#[test]
fn executerecordingcommandrequest_zero() {
    check::<spin_domain::ExecuteRecordingCommandRequest>(include_bytes!(
        "fixtures/ExecuteRecordingCommandRequest-zero.json"
    ));
}
#[test]
fn executerecordingcommandrequest_filled() {
    check::<spin_domain::ExecuteRecordingCommandRequest>(include_bytes!(
        "fixtures/ExecuteRecordingCommandRequest-filled.json"
    ));
}
#[test]
fn attachrecordingparentrequest_zero() {
    check::<spin_domain::AttachRecordingParentRequest>(include_bytes!(
        "fixtures/AttachRecordingParentRequest-zero.json"
    ));
}
#[test]
fn attachrecordingparentrequest_filled() {
    check::<spin_domain::AttachRecordingParentRequest>(include_bytes!(
        "fixtures/AttachRecordingParentRequest-filled.json"
    ));
}
#[test]
fn endrecordingrequest_zero() {
    check::<spin_domain::EndRecordingRequest>(include_bytes!(
        "fixtures/EndRecordingRequest-zero.json"
    ));
}
#[test]
fn endrecordingrequest_filled() {
    check::<spin_domain::EndRecordingRequest>(include_bytes!(
        "fixtures/EndRecordingRequest-filled.json"
    ));
}
#[test]
fn cancelrecordingrequest_zero() {
    check::<spin_domain::CancelRecordingRequest>(include_bytes!(
        "fixtures/CancelRecordingRequest-zero.json"
    ));
}
#[test]
fn cancelrecordingrequest_filled() {
    check::<spin_domain::CancelRecordingRequest>(include_bytes!(
        "fixtures/CancelRecordingRequest-filled.json"
    ));
}
#[test]
fn deleteartifactrequest_zero() {
    check::<spin_domain::DeleteArtifactRequest>(include_bytes!(
        "fixtures/DeleteArtifactRequest-zero.json"
    ));
}
#[test]
fn deleteartifactrequest_filled() {
    check::<spin_domain::DeleteArtifactRequest>(include_bytes!(
        "fixtures/DeleteArtifactRequest-filled.json"
    ));
}
#[test]
fn userequest_zero() {
    check::<spin_domain::UseRequest>(include_bytes!("fixtures/UseRequest-zero.json"));
}
#[test]
fn userequest_filled() {
    check::<spin_domain::UseRequest>(include_bytes!("fixtures/UseRequest-filled.json"));
}
#[test]
fn stopcompositionrequest_zero() {
    check::<spin_domain::StopCompositionRequest>(include_bytes!(
        "fixtures/StopCompositionRequest-zero.json"
    ));
}
#[test]
fn stopcompositionrequest_filled() {
    check::<spin_domain::StopCompositionRequest>(include_bytes!(
        "fixtures/StopCompositionRequest-filled.json"
    ));
}
#[test]
fn startstatus_zero() {
    check::<spin_domain::StartStatus>(include_bytes!("fixtures/StartStatus-zero.json"));
}
#[test]
fn startstatus_filled() {
    check::<spin_domain::StartStatus>(include_bytes!("fixtures/StartStatus-filled.json"));
}
#[test]
fn sealstatus_zero() {
    check::<spin_domain::SealStatus>(include_bytes!("fixtures/SealStatus-zero.json"));
}
#[test]
fn sealstatus_filled() {
    check::<spin_domain::SealStatus>(include_bytes!("fixtures/SealStatus-filled.json"));
}
#[test]
fn createjobrequest_zero() {
    check::<spin_domain::CreateJobRequest>(include_bytes!("fixtures/CreateJobRequest-zero.json"));
}
#[test]
fn createjobrequest_filled() {
    check::<spin_domain::CreateJobRequest>(include_bytes!("fixtures/CreateJobRequest-filled.json"));
}
#[test]
fn createjobattachmentrequest_zero() {
    check::<spin_domain::CreateJobAttachmentRequest>(include_bytes!(
        "fixtures/CreateJobAttachmentRequest-zero.json"
    ));
}
#[test]
fn createjobattachmentrequest_filled() {
    check::<spin_domain::CreateJobAttachmentRequest>(include_bytes!(
        "fixtures/CreateJobAttachmentRequest-filled.json"
    ));
}
#[test]
fn createjobresponse_zero() {
    check::<spin_domain::CreateJobResponse>(include_bytes!("fixtures/CreateJobResponse-zero.json"));
}
#[test]
fn createjobresponse_filled() {
    check::<spin_domain::CreateJobResponse>(include_bytes!(
        "fixtures/CreateJobResponse-filled.json"
    ));
}
#[test]
fn createjobsessionrequest_zero() {
    check::<spin_domain::CreateJobSessionRequest>(include_bytes!(
        "fixtures/CreateJobSessionRequest-zero.json"
    ));
}
#[test]
fn createjobsessionrequest_filled() {
    check::<spin_domain::CreateJobSessionRequest>(include_bytes!(
        "fixtures/CreateJobSessionRequest-filled.json"
    ));
}
#[test]
fn createjobsessionresponse_zero() {
    check::<spin_domain::CreateJobSessionResponse>(include_bytes!(
        "fixtures/CreateJobSessionResponse-zero.json"
    ));
}
#[test]
fn createjobsessionresponse_filled() {
    check::<spin_domain::CreateJobSessionResponse>(include_bytes!(
        "fixtures/CreateJobSessionResponse-filled.json"
    ));
}
#[test]
fn createworkflowtemplaterequest_zero() {
    check::<spin_domain::CreateWorkflowTemplateRequest>(include_bytes!(
        "fixtures/CreateWorkflowTemplateRequest-zero.json"
    ));
}
#[test]
fn createworkflowtemplaterequest_filled() {
    check::<spin_domain::CreateWorkflowTemplateRequest>(include_bytes!(
        "fixtures/CreateWorkflowTemplateRequest-filled.json"
    ));
}
#[test]
fn assignjobrequest_zero() {
    check::<spin_domain::AssignJobRequest>(include_bytes!("fixtures/AssignJobRequest-zero.json"));
}
#[test]
fn assignjobrequest_filled() {
    check::<spin_domain::AssignJobRequest>(include_bytes!("fixtures/AssignJobRequest-filled.json"));
}
#[test]
fn updatejobenvironmentrequest_zero() {
    check::<spin_domain::UpdateJobEnvironmentRequest>(include_bytes!(
        "fixtures/UpdateJobEnvironmentRequest-zero.json"
    ));
}
#[test]
fn updatejobenvironmentrequest_filled() {
    check::<spin_domain::UpdateJobEnvironmentRequest>(include_bytes!(
        "fixtures/UpdateJobEnvironmentRequest-filled.json"
    ));
}
#[test]
fn createdeliverablecommentrequest_zero() {
    check::<spin_domain::CreateDeliverableCommentRequest>(include_bytes!(
        "fixtures/CreateDeliverableCommentRequest-zero.json"
    ));
}
#[test]
fn createdeliverablecommentrequest_filled() {
    check::<spin_domain::CreateDeliverableCommentRequest>(include_bytes!(
        "fixtures/CreateDeliverableCommentRequest-filled.json"
    ));
}
#[test]
fn createcodereviewrequest_zero() {
    check::<spin_domain::CreateCodeReviewRequest>(include_bytes!(
        "fixtures/CreateCodeReviewRequest-zero.json"
    ));
}
#[test]
fn createcodereviewrequest_filled() {
    check::<spin_domain::CreateCodeReviewRequest>(include_bytes!(
        "fixtures/CreateCodeReviewRequest-filled.json"
    ));
}
#[test]
fn createcodereviewcommentrequest_zero() {
    check::<spin_domain::CreateCodeReviewCommentRequest>(include_bytes!(
        "fixtures/CreateCodeReviewCommentRequest-zero.json"
    ));
}
#[test]
fn createcodereviewcommentrequest_filled() {
    check::<spin_domain::CreateCodeReviewCommentRequest>(include_bytes!(
        "fixtures/CreateCodeReviewCommentRequest-filled.json"
    ));
}
#[test]
fn codereviewbundle_zero() {
    check::<spin_domain::CodeReviewBundle>(include_bytes!("fixtures/CodeReviewBundle-zero.json"));
}
#[test]
fn codereviewbundle_filled() {
    check::<spin_domain::CodeReviewBundle>(include_bytes!("fixtures/CodeReviewBundle-filled.json"));
}
#[test]
fn answerworkflowquestionrequest_zero() {
    check::<spin_domain::AnswerWorkflowQuestionRequest>(include_bytes!(
        "fixtures/AnswerWorkflowQuestionRequest-zero.json"
    ));
}
#[test]
fn answerworkflowquestionrequest_filled() {
    check::<spin_domain::AnswerWorkflowQuestionRequest>(include_bytes!(
        "fixtures/AnswerWorkflowQuestionRequest-filled.json"
    ));
}
#[test]
fn workflowadvance_zero() {
    check::<spin_domain::WorkflowAdvance>(include_bytes!("fixtures/WorkflowAdvance-zero.json"));
}
#[test]
fn workflowadvance_filled() {
    check::<spin_domain::WorkflowAdvance>(include_bytes!("fixtures/WorkflowAdvance-filled.json"));
}
#[test]
fn createmcpserverrequest_zero() {
    check::<spin_domain::CreateMCPServerRequest>(include_bytes!(
        "fixtures/CreateMCPServerRequest-zero.json"
    ));
}
#[test]
fn createmcpserverrequest_filled() {
    check::<spin_domain::CreateMCPServerRequest>(include_bytes!(
        "fixtures/CreateMCPServerRequest-filled.json"
    ));
}
#[test]
fn creategitrepositoryrequest_zero() {
    check::<spin_domain::CreateGitRepositoryRequest>(include_bytes!(
        "fixtures/CreateGitRepositoryRequest-zero.json"
    ));
}
#[test]
fn creategitrepositoryrequest_filled() {
    check::<spin_domain::CreateGitRepositoryRequest>(include_bytes!(
        "fixtures/CreateGitRepositoryRequest-filled.json"
    ));
}
#[test]
fn updategitrepositoryrequest_zero() {
    check::<spin_domain::UpdateGitRepositoryRequest>(include_bytes!(
        "fixtures/UpdateGitRepositoryRequest-zero.json"
    ));
}
#[test]
fn updategitrepositoryrequest_filled() {
    check::<spin_domain::UpdateGitRepositoryRequest>(include_bytes!(
        "fixtures/UpdateGitRepositoryRequest-filled.json"
    ));
}
#[test]
fn creategitrepositoryresponse_zero() {
    check::<spin_domain::CreateGitRepositoryResponse>(include_bytes!(
        "fixtures/CreateGitRepositoryResponse-zero.json"
    ));
}
#[test]
fn creategitrepositoryresponse_filled() {
    check::<spin_domain::CreateGitRepositoryResponse>(include_bytes!(
        "fixtures/CreateGitRepositoryResponse-filled.json"
    ));
}
#[test]
fn creategitaccountrequest_zero() {
    check::<spin_domain::CreateGitAccountRequest>(include_bytes!(
        "fixtures/CreateGitAccountRequest-zero.json"
    ));
}
#[test]
fn creategitaccountrequest_filled() {
    check::<spin_domain::CreateGitAccountRequest>(include_bytes!(
        "fixtures/CreateGitAccountRequest-filled.json"
    ));
}
#[test]
fn setupuserrequest_zero() {
    check::<spin_domain::SetupUserRequest>(include_bytes!("fixtures/SetupUserRequest-zero.json"));
}
#[test]
fn setupuserrequest_filled() {
    check::<spin_domain::SetupUserRequest>(include_bytes!("fixtures/SetupUserRequest-filled.json"));
}
#[test]
fn loginrequest_zero() {
    check::<spin_domain::LoginRequest>(include_bytes!("fixtures/LoginRequest-zero.json"));
}
#[test]
fn loginrequest_filled() {
    check::<spin_domain::LoginRequest>(include_bytes!("fixtures/LoginRequest-filled.json"));
}
#[test]
fn createuserrequest_zero() {
    check::<spin_domain::CreateUserRequest>(include_bytes!("fixtures/CreateUserRequest-zero.json"));
}
#[test]
fn createuserrequest_filled() {
    check::<spin_domain::CreateUserRequest>(include_bytes!(
        "fixtures/CreateUserRequest-filled.json"
    ));
}
#[test]
fn savegitoauthconfigurationrequest_zero() {
    check::<spin_domain::SaveGitOAuthConfigurationRequest>(include_bytes!(
        "fixtures/SaveGitOAuthConfigurationRequest-zero.json"
    ));
}
#[test]
fn savegitoauthconfigurationrequest_filled() {
    check::<spin_domain::SaveGitOAuthConfigurationRequest>(include_bytes!(
        "fixtures/SaveGitOAuthConfigurationRequest-filled.json"
    ));
}
#[test]
fn registerclientrequest_zero() {
    check::<spin_domain::RegisterClientRequest>(include_bytes!(
        "fixtures/RegisterClientRequest-zero.json"
    ));
}
#[test]
fn registerclientrequest_filled() {
    check::<spin_domain::RegisterClientRequest>(include_bytes!(
        "fixtures/RegisterClientRequest-filled.json"
    ));
}
#[test]
fn claimrequest_zero() {
    check::<spin_domain::ClaimRequest>(include_bytes!("fixtures/ClaimRequest-zero.json"));
}
#[test]
fn claimrequest_filled() {
    check::<spin_domain::ClaimRequest>(include_bytes!("fixtures/ClaimRequest-filled.json"));
}
#[test]
fn assignment_zero() {
    check::<spin_domain::Assignment>(include_bytes!("fixtures/Assignment-zero.json"));
}
#[test]
fn assignment_filled() {
    check::<spin_domain::Assignment>(include_bytes!("fixtures/Assignment-filled.json"));
}
#[test]
fn activationrequest_zero() {
    check::<spin_domain::ActivationRequest>(include_bytes!("fixtures/ActivationRequest-zero.json"));
}
#[test]
fn activationrequest_filled() {
    check::<spin_domain::ActivationRequest>(include_bytes!(
        "fixtures/ActivationRequest-filled.json"
    ));
}
#[test]
fn createturnrequest_zero() {
    check::<spin_domain::CreateTurnRequest>(include_bytes!("fixtures/CreateTurnRequest-zero.json"));
}
#[test]
fn createturnrequest_filled() {
    check::<spin_domain::CreateTurnRequest>(include_bytes!(
        "fixtures/CreateTurnRequest-filled.json"
    ));
}
#[test]
fn createcheckpointrequest_zero() {
    check::<spin_domain::CreateCheckpointRequest>(include_bytes!(
        "fixtures/CreateCheckpointRequest-zero.json"
    ));
}
#[test]
fn createcheckpointrequest_filled() {
    check::<spin_domain::CreateCheckpointRequest>(include_bytes!(
        "fixtures/CreateCheckpointRequest-filled.json"
    ));
}
#[test]
fn createresultrequest_zero() {
    check::<spin_domain::CreateResultRequest>(include_bytes!(
        "fixtures/CreateResultRequest-zero.json"
    ));
}
#[test]
fn createresultrequest_filled() {
    check::<spin_domain::CreateResultRequest>(include_bytes!(
        "fixtures/CreateResultRequest-filled.json"
    ));
}
#[test]
fn forksessionrequest_zero() {
    check::<spin_domain::ForkSessionRequest>(include_bytes!(
        "fixtures/ForkSessionRequest-zero.json"
    ));
}
#[test]
fn forksessionrequest_filled() {
    check::<spin_domain::ForkSessionRequest>(include_bytes!(
        "fixtures/ForkSessionRequest-filled.json"
    ));
}
#[test]
fn selectresultrequest_zero() {
    check::<spin_domain::SelectResultRequest>(include_bytes!(
        "fixtures/SelectResultRequest-zero.json"
    ));
}
#[test]
fn selectresultrequest_filled() {
    check::<spin_domain::SelectResultRequest>(include_bytes!(
        "fixtures/SelectResultRequest-filled.json"
    ));
}
#[test]
fn execution_zero() {
    check::<spin_domain::engine::Execution>(include_bytes!("fixtures/Execution-zero.json"));
}
#[test]
fn execution_filled() {
    check::<spin_domain::engine::Execution>(include_bytes!("fixtures/Execution-filled.json"));
}
#[test]
fn recordingstack_zero() {
    check::<spin_domain::engine::RecordingStack>(include_bytes!(
        "fixtures/RecordingStack-zero.json"
    ));
}
#[test]
fn recordingstack_filled() {
    check::<spin_domain::engine::RecordingStack>(include_bytes!(
        "fixtures/RecordingStack-filled.json"
    ));
}
#[test]
fn livecapsules_zero() {
    check::<spin_domain::engine::LiveCapsules>(include_bytes!("fixtures/LiveCapsules-zero.json"));
}
#[test]
fn livecapsules_filled() {
    check::<spin_domain::engine::LiveCapsules>(include_bytes!("fixtures/LiveCapsules-filled.json"));
}
#[test]
fn gitauthentication_zero() {
    check::<spin_domain::engine::GitAuthentication>(include_bytes!(
        "fixtures/GitAuthentication-zero.json"
    ));
}
#[test]
fn gitauthentication_filled() {
    check::<spin_domain::engine::GitAuthentication>(include_bytes!(
        "fixtures/GitAuthentication-filled.json"
    ));
}
#[test]
fn workspacefilechange_zero() {
    check::<spin_domain::engine::WorkspaceFileChange>(include_bytes!(
        "fixtures/WorkspaceFileChange-zero.json"
    ));
}
#[test]
fn workspacefilechange_filled() {
    check::<spin_domain::engine::WorkspaceFileChange>(include_bytes!(
        "fixtures/WorkspaceFileChange-filled.json"
    ));
}
#[test]
fn workspacechanges_zero() {
    check::<spin_domain::engine::WorkspaceChanges>(include_bytes!(
        "fixtures/WorkspaceChanges-zero.json"
    ));
}
#[test]
fn workspacechanges_filled() {
    check::<spin_domain::engine::WorkspaceChanges>(include_bytes!(
        "fixtures/WorkspaceChanges-filled.json"
    ));
}
#[test]
fn workspaceattachment_zero() {
    check::<spin_domain::engine::WorkspaceAttachment>(include_bytes!(
        "fixtures/WorkspaceAttachment-zero.json"
    ));
}
#[test]
fn workspaceattachment_filled() {
    check::<spin_domain::engine::WorkspaceAttachment>(include_bytes!(
        "fixtures/WorkspaceAttachment-filled.json"
    ));
}
#[test]
fn trackedselection_zero() {
    check::<spin_domain::engine::TrackedSelection>(include_bytes!(
        "fixtures/TrackedSelection-zero.json"
    ));
}
#[test]
fn trackedselection_filled() {
    check::<spin_domain::engine::TrackedSelection>(include_bytes!(
        "fixtures/TrackedSelection-filled.json"
    ));
}
#[test]
fn workspacecomparison_zero() {
    check::<spin_domain::engine::WorkspaceComparison>(include_bytes!(
        "fixtures/WorkspaceComparison-zero.json"
    ));
}
#[test]
fn workspacecomparison_filled() {
    check::<spin_domain::engine::WorkspaceComparison>(include_bytes!(
        "fixtures/WorkspaceComparison-filled.json"
    ));
}
#[test]
fn workspaceacceptance_zero() {
    check::<spin_domain::engine::WorkspaceAcceptance>(include_bytes!(
        "fixtures/WorkspaceAcceptance-zero.json"
    ));
}
#[test]
fn workspaceacceptance_filled() {
    check::<spin_domain::engine::WorkspaceAcceptance>(include_bytes!(
        "fixtures/WorkspaceAcceptance-filled.json"
    ));
}
#[test]
fn workspacemerge_zero() {
    check::<spin_domain::engine::WorkspaceMerge>(include_bytes!(
        "fixtures/WorkspaceMerge-zero.json"
    ));
}
#[test]
fn workspacemerge_filled() {
    check::<spin_domain::engine::WorkspaceMerge>(include_bytes!(
        "fixtures/WorkspaceMerge-filled.json"
    ));
}
#[test]
fn workspacemergeresult_zero() {
    check::<spin_domain::engine::WorkspaceMergeResult>(include_bytes!(
        "fixtures/WorkspaceMergeResult-zero.json"
    ));
}
#[test]
fn workspacemergeresult_filled() {
    check::<spin_domain::engine::WorkspaceMergeResult>(include_bytes!(
        "fixtures/WorkspaceMergeResult-filled.json"
    ));
}
#[test]
fn workspacesync_zero() {
    check::<spin_domain::engine::WorkspaceSync>(include_bytes!("fixtures/WorkspaceSync-zero.json"));
}
#[test]
fn workspacesync_filled() {
    check::<spin_domain::engine::WorkspaceSync>(include_bytes!(
        "fixtures/WorkspaceSync-filled.json"
    ));
}
#[test]
fn workspacesyncresult_zero() {
    check::<spin_domain::engine::WorkspaceSyncResult>(include_bytes!(
        "fixtures/WorkspaceSyncResult-zero.json"
    ));
}
#[test]
fn workspacesyncresult_filled() {
    check::<spin_domain::engine::WorkspaceSyncResult>(include_bytes!(
        "fixtures/WorkspaceSyncResult-filled.json"
    ));
}
#[test]
fn workspaceentry_zero() {
    check::<spin_domain::engine::WorkspaceEntry>(include_bytes!(
        "fixtures/WorkspaceEntry-zero.json"
    ));
}
#[test]
fn workspaceentry_filled() {
    check::<spin_domain::engine::WorkspaceEntry>(include_bytes!(
        "fixtures/WorkspaceEntry-filled.json"
    ));
}
#[test]
fn workspacetree_zero() {
    check::<spin_domain::engine::WorkspaceTree>(include_bytes!("fixtures/WorkspaceTree-zero.json"));
}
#[test]
fn workspacetree_filled() {
    check::<spin_domain::engine::WorkspaceTree>(include_bytes!(
        "fixtures/WorkspaceTree-filled.json"
    ));
}
#[test]
fn workspacefile_zero() {
    check::<spin_domain::engine::WorkspaceFile>(include_bytes!("fixtures/WorkspaceFile-zero.json"));
}
#[test]
fn workspacefile_filled() {
    check::<spin_domain::engine::WorkspaceFile>(include_bytes!(
        "fixtures/WorkspaceFile-filled.json"
    ));
}
#[test]
fn repositorybrowse_zero() {
    check::<spin_domain::engine::RepositoryBrowse>(include_bytes!(
        "fixtures/RepositoryBrowse-zero.json"
    ));
}
#[test]
fn repositorybrowse_filled() {
    check::<spin_domain::engine::RepositoryBrowse>(include_bytes!(
        "fixtures/RepositoryBrowse-filled.json"
    ));
}
#[test]
fn repositoryref_zero() {
    check::<spin_domain::engine::RepositoryRef>(include_bytes!("fixtures/RepositoryRef-zero.json"));
}
#[test]
fn repositoryref_filled() {
    check::<spin_domain::engine::RepositoryRef>(include_bytes!(
        "fixtures/RepositoryRef-filled.json"
    ));
}
#[test]
fn repositorybrowseresult_zero() {
    check::<spin_domain::engine::RepositoryBrowseResult>(include_bytes!(
        "fixtures/RepositoryBrowseResult-zero.json"
    ));
}
#[test]
fn repositorybrowseresult_filled() {
    check::<spin_domain::engine::RepositoryBrowseResult>(include_bytes!(
        "fixtures/RepositoryBrowseResult-filled.json"
    ));
}
#[test]
fn repositorycomparison_zero() {
    check::<spin_domain::engine::RepositoryComparison>(include_bytes!(
        "fixtures/RepositoryComparison-zero.json"
    ));
}
#[test]
fn repositorycomparison_filled() {
    check::<spin_domain::engine::RepositoryComparison>(include_bytes!(
        "fixtures/RepositoryComparison-filled.json"
    ));
}
#[test]
fn repositoryacceptance_zero() {
    check::<spin_domain::engine::RepositoryAcceptance>(include_bytes!(
        "fixtures/RepositoryAcceptance-zero.json"
    ));
}
#[test]
fn repositoryacceptance_filled() {
    check::<spin_domain::engine::RepositoryAcceptance>(include_bytes!(
        "fixtures/RepositoryAcceptance-filled.json"
    ));
}
#[test]
fn repositorymerge_zero() {
    check::<spin_domain::engine::RepositoryMerge>(include_bytes!(
        "fixtures/RepositoryMerge-zero.json"
    ));
}
#[test]
fn repositorymerge_filled() {
    check::<spin_domain::engine::RepositoryMerge>(include_bytes!(
        "fixtures/RepositoryMerge-filled.json"
    ));
}
#[test]
fn workspaceacceptanceresult_zero() {
    check::<spin_domain::engine::WorkspaceAcceptanceResult>(include_bytes!(
        "fixtures/WorkspaceAcceptanceResult-zero.json"
    ));
}
#[test]
fn workspaceacceptanceresult_filled() {
    check::<spin_domain::engine::WorkspaceAcceptanceResult>(include_bytes!(
        "fixtures/WorkspaceAcceptanceResult-filled.json"
    ));
}
