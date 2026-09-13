package store

import (
	"fmt"
	"slices"
	"strconv"
	"strings"
	"time"

	"easyacp/internal/domain"
)

const maxDeliverableBytes = 2 << 20

func (s *Store) CreateWorkflowTemplate(req domain.CreateWorkflowTemplateRequest) (domain.WorkflowTemplate, error) {
	operator, name, description, phases, err := normalizeWorkflowTemplateRequest(req)
	if err != nil {
		return domain.WorkflowTemplate{}, err
	}
	gitSelector, err := normalizeTemplateGitSelector(req.GitSelector)
	if err != nil {
		return domain.WorkflowTemplate{}, err
	}
	now := time.Now().UTC()
	template := domain.WorkflowTemplate{
		ID: newID("tpl"), Revision: 1, Name: name, Description: description, CreatedBy: operator,
		GitSelector: gitSelector, Phases: phases, CreatedAt: now, UpdatedAt: now,
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if gitSelector == "" {
		gitSelector, err = s.defaultEnabledSelectorLocked(operator, "git", "default")
		if err != nil {
			return domain.WorkflowTemplate{}, fmt.Errorf("Template Git environment: %w", err)
		}
		template.GitSelector = gitSelector
	}
	if err := s.validateDirectEnabledSelectorLocked(operator, gitSelector, "git", "default"); err != nil {
		return domain.WorkflowTemplate{}, fmt.Errorf("Template Git environment: %w", err)
	}
	for _, existing := range s.state.WorkflowTemplates {
		if strings.EqualFold(existing.Name, template.Name) {
			return domain.WorkflowTemplate{}, fmt.Errorf("a template named %q already exists: %w", template.Name, ErrConflict)
		}
	}
	s.state.WorkflowTemplates[template.ID] = template
	return template, s.saveLocked()
}

func normalizeWorkflowTemplateRequest(req domain.CreateWorkflowTemplateRequest) (string, string, string, []domain.WorkflowPhase, error) {
	operator := normalizeSubject(req.Operator)
	name := strings.TrimSpace(req.Name)
	inputs := make([]domain.WorkflowPhase, 0, len(req.Phases))
	for _, input := range req.Phases {
		if input.Executor == domain.WorkflowExecutorAction || input.Action != nil {
			if input.Action == nil || !isWorkflowActionType(input.Action.Type) {
				return "", "", "", nil, fmt.Errorf("a step Spin performs itself is a merge or a pull request: %w", ErrConflict)
			}
			input.Executor = domain.WorkflowExecutorAction
		}
		inputs = append(inputs, input)
	}
	if operator == "" || name == "" || len(inputs) == 0 {
		return "", "", "", nil, fmt.Errorf("operator, name and at least one phase are required: %w", ErrConflict)
	}
	phases := make([]domain.WorkflowPhase, len(inputs))
	phaseIDs := map[string]bool{}
	availableDeliverables := map[string]string{}
	for index, input := range inputs {
		phase := input
		if phase.Executor == "" {
			phase.Executor = domain.WorkflowExecutorAgent
		}
		phase.Model = strings.TrimSpace(phase.Model)
		phase.ReasoningEffort = strings.TrimSpace(phase.ReasoningEffort)
		phase.Name = strings.TrimSpace(phase.Name)
		phase.Instructions = strings.TrimSpace(phase.Instructions)
		phase.ID = normalizeName(phase.ID)
		if phase.ID == "" {
			phase.ID = fmt.Sprintf("step-%d", index+1)
		}
		if !validToken(phase.ID) || phaseIDs[phase.ID] || phase.Name == "" {
			return "", "", "", nil, fmt.Errorf("phase %d needs a unique id and name: %w", index+1, ErrConflict)
		}
		phaseIDs[phase.ID] = true
		switch phase.Executor {
		case domain.WorkflowExecutorAgent:
			if phase.Instructions == "" {
				return "", "", "", nil, fmt.Errorf("agent phase %s needs instructions: %w", phase.Name, ErrConflict)
			}
			phase.Action = nil
			phase.EnvironmentSelector = strings.ToLower(strings.TrimSpace(phase.EnvironmentSelector))
			if phase.EnvironmentSelector != "" {
				if _, _, err := parseArtifactSelector(phase.EnvironmentSelector); err != nil {
					return "", "", "", nil, fmt.Errorf("phase %s environment: %w", phase.Name, err)
				}
			}
			withSelectors, err := normalizeArtifactSelectors(phase.WithSelectors)
			if err != nil {
				return "", "", "", nil, fmt.Errorf("phase %s WITH layers: %w", phase.Name, err)
			}
			phase.WithSelectors = withSelectors
		case domain.WorkflowExecutorExpose:
			// The app is started on the phase's workspace and a person tests
			// it; there is no agent to instruct.
			phase.Action = nil
			phase.EnvironmentSelector = strings.ToLower(strings.TrimSpace(phase.EnvironmentSelector))
			phase.WithSelectors = nil
			phase.Model, phase.ReasoningEffort = "", ""
			phase.Deliverables = nil
		case domain.WorkflowExecutorAction:
			// A step Spin performs itself: it merges the Job branch into
			// the base branch with the operator's credentials, or opens a
			// pull request on the remote. Its transitions are the person's
			// like any step's; a failure is a plain reject, so the next
			// step reads it as feedback.
			if phase.Action == nil || !isWorkflowActionType(phase.Action.Type) {
				return "", "", "", nil, fmt.Errorf("phase %s: a step Spin performs itself is a merge or a pull request: %w", phase.Name, ErrConflict)
			}
			phase.Action = &domain.WorkflowAction{Type: strings.ToLower(strings.TrimSpace(phase.Action.Type))}
			phase.EnvironmentSelector, phase.WithSelectors, phase.Model, phase.ReasoningEffort = "", nil, "", ""
			phase.Deliverables, phase.Inject, phase.AllowChanges = nil, nil, false
		default:
			return "", "", "", nil, fmt.Errorf("phase %s has unsupported executor %q: %w", phase.Name, phase.Executor, ErrConflict)
		}
		injected := make([]string, 0, len(phase.Inject))
		seenInjected := map[string]bool{}
		for _, requested := range phase.Inject {
			key := strings.ToLower(strings.TrimSpace(requested))
			canonical, ok := availableDeliverables[key]
			if key == "" || !ok {
				return "", "", "", nil, fmt.Errorf("phase %s injects unknown earlier deliverable %q: %w", phase.Name, requested, ErrConflict)
			}
			if seenInjected[key] {
				return "", "", "", nil, fmt.Errorf("phase %s injects deliverable %q more than once: %w", phase.Name, requested, ErrConflict)
			}
			seenInjected[key] = true
			injected = append(injected, canonical)
		}
		phase.Inject = injected
		seenDeliverables := map[string]bool{}
		phase.Deliverables = append([]domain.DeliverableDefinition{}, phase.Deliverables...)
		for deliverableIndex := range phase.Deliverables {
			deliverable := &phase.Deliverables[deliverableIndex]
			deliverable.Name = strings.TrimSpace(deliverable.Name)
			deliverable.Description = strings.TrimSpace(deliverable.Description)
			deliverable.Kind = strings.ToLower(strings.TrimSpace(deliverable.Kind))
			if deliverable.Kind == "" {
				deliverable.Kind = domain.DeliverableKindMarkdown
			}
			if !slices.Contains(domain.DeliverableKinds, deliverable.Kind) {
				return "", "", "", nil, fmt.Errorf("deliverable %s has unknown kind %q: %w", deliverable.Name, deliverable.Kind, ErrConflict)
			}
			key := strings.ToLower(deliverable.Name)
			if key == "" || seenDeliverables[key] {
				return "", "", "", nil, fmt.Errorf("phase %s has an empty or duplicate deliverable: %w", phase.Name, ErrConflict)
			}
			seenDeliverables[key] = true
			if _, exists := availableDeliverables[key]; !exists {
				availableDeliverables[key] = deliverable.Name
			}
		}
		phases[index] = phase
	}
	for index := range phases {
		phase := &phases[index]
		if strings.TrimSpace(phase.Accept.Target) == "" {
			phase.Accept.Target = domain.WorkflowTargetNext
		}
		if strings.TrimSpace(phase.Reject.Target) == "" {
			phase.Reject.Target = domain.WorkflowTargetSelf
		}
		if phase.Reject.Max < 0 || phase.Accept.Max < 0 {
			return "", "", "", nil, fmt.Errorf("transition max cannot be negative: %w", ErrConflict)
		}
		if phase.Reject.Max > 0 && strings.TrimSpace(phase.Reject.Exhausted) == "" {
			phase.Reject.Exhausted = domain.WorkflowTargetAskUser
		}
		for _, transition := range []domain.WorkflowTransition{phase.Accept, phase.Reject} {
			for _, target := range []string{transition.Target, transition.Exhausted} {
				if target != "" && !validWorkflowTarget(target, phaseIDs) {
					return "", "", "", nil, fmt.Errorf("phase %s points to unknown target %q: %w", phase.Name, target, ErrConflict)
				}
			}
		}
	}
	return operator, name, strings.TrimSpace(req.Description), phases, nil
}

// isWorkflowActionType names what Spin performs itself as a step.
func isWorkflowActionType(actionType string) bool {
	actionType = strings.ToLower(strings.TrimSpace(actionType))
	return actionType == domain.WorkflowActionGitPullRequest || actionType == domain.WorkflowActionGitMerge
}

func (s *Store) UpdateWorkflowTemplate(templateID string, req domain.CreateWorkflowTemplateRequest) (domain.WorkflowTemplate, error) {
	operator, name, description, phases, err := normalizeWorkflowTemplateRequest(req)
	if err != nil {
		return domain.WorkflowTemplate{}, err
	}
	gitSelector, err := normalizeTemplateGitSelector(req.GitSelector)
	if err != nil {
		return domain.WorkflowTemplate{}, err
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if gitSelector == "" {
		gitSelector, err = s.defaultEnabledSelectorLocked(operator, "git", "default")
		if err != nil {
			return domain.WorkflowTemplate{}, fmt.Errorf("Template Git environment: %w", err)
		}
	}
	if err := s.validateDirectEnabledSelectorLocked(operator, gitSelector, "git", "default"); err != nil {
		return domain.WorkflowTemplate{}, fmt.Errorf("Template Git environment: %w", err)
	}
	template, exists := s.state.WorkflowTemplates[strings.TrimSpace(templateID)]
	if !exists {
		return domain.WorkflowTemplate{}, ErrNotFound
	}
	if template.CreatedBy != operator {
		return domain.WorkflowTemplate{}, ErrConflict
	}
	for _, existing := range s.state.WorkflowTemplates {
		if existing.ID != template.ID && strings.EqualFold(existing.Name, name) {
			return domain.WorkflowTemplate{}, fmt.Errorf("a template named %q already exists: %w", name, ErrConflict)
		}
	}
	template.Name = name
	template.Description = description
	template.GitSelector = gitSelector
	template.Phases = phases
	template.Revision++
	if template.Revision < 1 {
		template.Revision = 1
	}
	template.UpdatedAt = time.Now().UTC()
	s.state.WorkflowTemplates[template.ID] = template
	return template, s.saveLocked()
}

func normalizeTemplateGitSelector(selector string) (string, error) {
	selector = strings.ToLower(strings.TrimSpace(selector))
	if selector == "" {
		return "", nil
	}
	if _, _, err := parseArtifactSelector(selector); err != nil {
		return "", fmt.Errorf("invalid git_selector: %w", err)
	}
	return selector, nil
}

func validWorkflowTarget(target string, phaseIDs map[string]bool) bool {
	target = strings.TrimSpace(target)
	switch strings.ToUpper(target) {
	case domain.WorkflowTargetNext, domain.WorkflowTargetSelf, domain.WorkflowTargetDone, domain.WorkflowTargetAskUser:
		return true
	}
	return phaseIDs[normalizeName(target)]
}

func (s *Store) DeleteWorkflowTemplate(templateID, operator string) (domain.WorkflowTemplate, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	template, ok := s.state.WorkflowTemplates[templateID]
	if !ok {
		return domain.WorkflowTemplate{}, ErrNotFound
	}
	if template.CreatedBy != normalizeSubject(operator) {
		return domain.WorkflowTemplate{}, ErrConflict
	}
	for _, job := range s.state.Jobs {
		if job.TemplateID == template.ID {
			return domain.WorkflowTemplate{}, fmt.Errorf("template is used by job %s: %w", job.ID, ErrConflict)
		}
	}
	delete(s.state.WorkflowTemplates, template.ID)
	return template, s.saveLocked()
}

func (s *Store) WorkflowForSession(sessionID string) (domain.Job, domain.WorkflowTemplate, domain.PhaseRun, domain.WorkflowPhase, []domain.Deliverable, []domain.WorkflowQuestion, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	session, ok := s.state.Sessions[sessionID]
	if !ok || session.PhaseRunID == "" {
		return domain.Job{}, domain.WorkflowTemplate{}, domain.PhaseRun{}, domain.WorkflowPhase{}, nil, nil, ErrNotFound
	}
	job, template, run, phase, err := s.workflowLocked(session)
	if err != nil {
		return domain.Job{}, domain.WorkflowTemplate{}, domain.PhaseRun{}, domain.WorkflowPhase{}, nil, nil, err
	}
	deliverables := make([]domain.Deliverable, 0)
	for _, deliverable := range s.state.Deliverables {
		if deliverable.JobID == job.ID {
			deliverables = append(deliverables, deliverable)
		}
	}
	questions := make([]domain.WorkflowQuestion, 0)
	for _, question := range s.state.WorkflowQuestions {
		if question.JobID == job.ID {
			questions = append(questions, question)
		}
	}
	slices.SortFunc(deliverables, func(a, b domain.Deliverable) int { return a.CreatedAt.Compare(b.CreatedAt) })
	slices.SortFunc(questions, func(a, b domain.WorkflowQuestion) int { return a.CreatedAt.Compare(b.CreatedAt) })
	return job, template, run, phase, deliverables, questions, nil
}

func (s *Store) workflowLocked(session domain.Session) (domain.Job, domain.WorkflowTemplate, domain.PhaseRun, domain.WorkflowPhase, error) {
	job, ok := s.state.Jobs[session.JobID]
	if !ok {
		return domain.Job{}, domain.WorkflowTemplate{}, domain.PhaseRun{}, domain.WorkflowPhase{}, ErrNotFound
	}
	template, ok := s.workflowTemplateForJobLocked(job)
	if !ok {
		return domain.Job{}, domain.WorkflowTemplate{}, domain.PhaseRun{}, domain.WorkflowPhase{}, ErrNotFound
	}
	run, ok := s.state.PhaseRuns[session.PhaseRunID]
	if !ok || run.SessionID != session.ID || run.JobID != job.ID {
		return domain.Job{}, domain.WorkflowTemplate{}, domain.PhaseRun{}, domain.WorkflowPhase{}, ErrNotFound
	}
	phase, ok := workflowPhase(template, run.PhaseID)
	if !ok {
		return domain.Job{}, domain.WorkflowTemplate{}, domain.PhaseRun{}, domain.WorkflowPhase{}, ErrNotFound
	}
	return job, template, run, phase, nil
}

func (s *Store) workflowTemplateForJobLocked(job domain.Job) (domain.WorkflowTemplate, bool) {
	if job.TemplateSnapshot != nil {
		return cloneWorkflowTemplate(*job.TemplateSnapshot), true
	}
	template, ok := s.state.WorkflowTemplates[job.TemplateID]
	return cloneWorkflowTemplate(template), ok
}

func cloneWorkflowTemplate(template domain.WorkflowTemplate) domain.WorkflowTemplate {
	clone := template
	clone.Phases = make([]domain.WorkflowPhase, len(template.Phases))
	for index, phase := range template.Phases {
		clone.Phases[index] = phase
		clone.Phases[index].Inject = append([]string(nil), phase.Inject...)
		clone.Phases[index].WithSelectors = append([]string(nil), phase.WithSelectors...)
		clone.Phases[index].Deliverables = append([]domain.DeliverableDefinition(nil), phase.Deliverables...)
		if phase.Action != nil {
			action := *phase.Action
			clone.Phases[index].Action = &action
		}
	}
	return clone
}

func workflowPhase(template domain.WorkflowTemplate, phaseID string) (domain.WorkflowPhase, bool) {
	if phaseID == domain.BrainstormPhaseID {
		return domain.BrainstormPhase(), true
	}
	for _, phase := range template.Phases {
		if phase.ID == phaseID {
			return phase, true
		}
	}
	return domain.WorkflowPhase{}, false
}

// phaseEnvironmentLocked resolves a phase's environment against the Job's.
// A phase that names a layer without an agent (a Git or tool layer) is not
// asking to run without one: the Job's environment stays the entry, which
// is where a person chooses the model, and the phase's layer comes along
// as an extra WITH layer.
// phaseEnvironmentLocked is the stack a phase runs: the Job's layers, then
// what the phase adds on top. A phase layer with an agent goes on as the
// worker's credential layer for that agent (or the layer itself), so the
// topmost agent is the phase's; a layer the stack already holds, as itself
// or as another version, is not added twice. A layer without an agent is
// an extra toolset and leaves the Job's agent on top.
func (s *Store) phaseEnvironmentLocked(operator string, phase domain.WorkflowPhase, jobSelector string, jobWith []string) (string, []string) {
	selector, with := workflowPhaseLayers(phase, jobSelector, jobWith)
	if phase.EnvironmentSelector == "" || phase.EnvironmentSelector == jobSelector {
		return selector, with
	}
	artifact, err := s.resolveArtifactSelectorLocked(phase.EnvironmentSelector, operator, "default")
	if err != nil {
		return selector, uniqueStrings(append(with, phase.EnvironmentSelector))
	}
	layer := phase.EnvironmentSelector
	if s.artifactEnablesLocked(artifact.ID, "acp") {
		artifact = s.identityLayerLocked(artifact, operator)
		layer = string(artifact.Kind) + ":" + artifact.Name
	}
	for _, present := range append([]string{jobSelector}, with...) {
		held, err := s.resolveArtifactSelectorLocked(present, operator, "default")
		if err != nil {
			continue
		}
		if held.ID == artifact.ID || s.sameLineageLocked(held, artifact) || s.dependsOnLineageLocked(held.ID, artifact) {
			return selector, with
		}
	}
	return selector, uniqueStrings(append(with, layer))
}

// workflowPhaseLayers is the phase's stack before agents are considered:
// the Job's layer, the Job's further layers, then the phase's own.
func workflowPhaseLayers(phase domain.WorkflowPhase, jobSelector string, jobWith []string) (string, []string) {
	with := append([]string(nil), jobWith...)
	with = append(with, phase.WithSelectors...)
	return jobSelector, uniqueStrings(with)
}

// stackAgentToolLocked names the agent of a stack: the tool under the
// topmost layer that enables acp, or the bottom layer's tool.
func (s *Store) stackAgentToolLocked(operator, selector string, with []string) string {
	selectors := append([]string{selector}, with...)
	for index := len(selectors) - 1; index >= 0; index-- {
		artifact, err := s.resolveArtifactSelectorLocked(selectors[index], operator, "default")
		if err != nil {
			continue
		}
		if enabling, ok := s.enablingLayerLocked(artifact.ID, "acp"); ok {
			return enabling.Name
		}
	}
	_, tool, _ := parseArtifactSelector(selector)
	return tool
}

// RequeueWorkflowPhase puts a running phase back in the queue when its agent
// could not be started after all; the launch sweep offers it again and the
// Job shows why instead of a "running" step with nothing behind it.
func (s *Store) RequeueWorkflowPhase(sessionID string) (domain.PhaseRun, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	session, ok := s.state.Sessions[sessionID]
	if !ok || session.PhaseRunID == "" {
		return domain.PhaseRun{}, ErrNotFound
	}
	run, ok := s.state.PhaseRuns[session.PhaseRunID]
	if !ok {
		return domain.PhaseRun{}, ErrNotFound
	}
	if run.Status == domain.PhaseRunRunning {
		run.Status = domain.PhaseRunQueued
		s.state.PhaseRuns[run.ID] = run
		if err := s.saveLocked(); err != nil {
			return domain.PhaseRun{}, err
		}
	}
	return run, nil
}

func (s *Store) MarkWorkflowPhaseRunning(sessionID string) (domain.PhaseRun, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	session, ok := s.state.Sessions[sessionID]
	if !ok || session.PhaseRunID == "" {
		return domain.PhaseRun{}, ErrNotFound
	}
	run, ok := s.state.PhaseRuns[session.PhaseRunID]
	if !ok {
		return domain.PhaseRun{}, ErrNotFound
	}
	if run.Status == domain.PhaseRunQueued {
		run.Status = domain.PhaseRunRunning
		run.PendingReason = ""
		s.state.PhaseRuns[run.ID] = run
		job := s.state.Jobs[run.JobID]
		job.WorkflowStatus = domain.WorkflowBusy
		job.PendingReason = ""
		job.UpdatedAt = time.Now().UTC()
		s.state.Jobs[job.ID] = job
		if err := s.saveLocked(); err != nil {
			return domain.PhaseRun{}, err
		}
	}
	return run, nil
}

// RetryWorkflowSession returns the current workflow phase to its queue without
// creating a new phase attempt or Session. Runtime cleanup is deliberately a
// server concern: the Store only records the explicit user decision.
func (s *Store) RetryWorkflowSession(sessionID, operator, note string, transcript []domain.ChatLine) (domain.CreateJobResponse, string, error) {
	operator = normalizeSubject(operator)
	note = strings.TrimSpace(note)
	if len(note) > 4000 {
		return domain.CreateJobResponse{}, "", fmt.Errorf("the note for the new attempt exceeds 4000 characters: %w", ErrConflict)
	}
	// The conversation that comes along is bounded: the last 60 messages,
	// each cut to 4000 characters.
	if len(transcript) > 60 {
		transcript = transcript[len(transcript)-60:]
	}
	kept := make([]domain.ChatLine, 0, len(transcript))
	for _, line := range transcript {
		text := strings.TrimSpace(line.Text)
		if text == "" {
			continue
		}
		if len(text) > 4000 {
			text = text[:4000] + "…"
		}
		role := "agent"
		if line.Role == "user" {
			role = "user"
		}
		kept = append(kept, domain.ChatLine{Role: role, Text: text})
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	session, ok := s.state.Sessions[strings.TrimSpace(sessionID)]
	if !ok || session.PhaseRunID == "" {
		return domain.CreateJobResponse{}, "", ErrNotFound
	}
	job, _, run, _, err := s.workflowLocked(session)
	if err != nil {
		return domain.CreateJobResponse{}, "", err
	}
	if !job.AllowsOperator(operator) {
		return domain.CreateJobResponse{}, "", fmt.Errorf("only the owner or assignee of the Job can retry it: %w", ErrConflict)
	}
	if job.CurrentPhaseRunID != run.ID || job.Status == domain.JobDone || job.Status == domain.JobCancelled {
		return domain.CreateJobResponse{}, "", fmt.Errorf("only the active workflow Session can be retried: %w", ErrConflict)
	}
	switch run.Status {
	case domain.PhaseRunQueued, domain.PhaseRunRunning, domain.PhaseRunPending:
	default:
		return domain.CreateJobResponse{}, "", fmt.Errorf("phase is %s and cannot be retried: %w", run.Status, ErrConflict)
	}

	now := time.Now().UTC()
	for id, question := range s.state.WorkflowQuestions {
		if question.PhaseRunID != run.ID || question.Status != "open" {
			continue
		}
		question.Status = "answered"
		question.Answer = "retry"
		question.Reason = "Session retried by user"
		question.AnsweredBy = operator
		question.AnsweredAt = &now
		s.state.WorkflowQuestions[id] = question
	}
	run.Status = domain.PhaseRunQueued
	run.PendingReason = ""
	run.PendingOutcome = ""
	run.Summary = ""
	run.RejectReason = ""
	run.CompletedAt = nil
	run.Restarts++
	if note != "" {
		run.RestartNotes = append(run.RestartNotes, note)
	}
	run.RestartTranscript = kept
	job.Status = domain.JobActive
	job.WorkflowStatus = domain.WorkflowBusy
	job.PendingReason = ""
	job.UpdatedAt = now
	session.Status = domain.SessionQueued
	previousCompositionID := session.PreparedCompositionID
	session.PreparedCompositionID = ""
	session.ClientID = ""
	session.ActivationID = ""
	session.LeaseExpiresAt = nil
	session.BaseRef = job.Branch
	session.TargetBranch = job.Branch
	session.UpdatedAt = now

	s.state.PhaseRuns[run.ID] = run
	s.state.Jobs[job.ID] = job
	s.state.Sessions[session.ID] = session
	if err := s.saveLocked(); err != nil {
		return domain.CreateJobResponse{}, "", err
	}
	return domain.CreateJobResponse{Job: job, Session: session}, previousCompositionID, nil
}

// PutWorkflowDeliverable stores what the agent put: a Markdown document
// as text, a visual deliverable as the bundle the runner delivered. Either
// becomes the revision of this step, as storeDeliverableLocked says.
func (s *Store) PutWorkflowDeliverable(sessionID, name, content string, bundle *domain.DeliverableBundle) (domain.Deliverable, error) {
	name = strings.TrimSpace(name)
	content = strings.TrimSpace(content)
	s.mu.Lock()
	defer s.mu.Unlock()
	job, run, definition, err := s.deliverableTargetLocked(sessionID, name)
	if err != nil {
		return domain.Deliverable{}, err
	}
	if err := checkDeliverableShape(definition, content, bundle); err != nil {
		return domain.Deliverable{}, err
	}
	if domain.DeliverableIsBundle(definition.Kind) {
		content = ""
	}
	return s.storeDeliverableLocked(job, run, sessionID, definition, content, bundle)
}

// checkDeliverableShape is the measure of a delivery: what was put must
// be what the definition asks for.
func checkDeliverableShape(definition domain.DeliverableDefinition, content string, bundle *domain.DeliverableBundle) error {
	kind := definition.Kind
	if kind == "" {
		kind = domain.DeliverableKindMarkdown
	}
	if !domain.DeliverableIsBundle(kind) {
		if bundle != nil || content == "" || len(content) > maxDeliverableBytes {
			return fmt.Errorf("deliverable %s is a Markdown document of at most %d bytes: %w", definition.Name, maxDeliverableBytes, ErrConflict)
		}
		return nil
	}
	if bundle == nil || bundle.Ref == "" || bundle.Files < 1 {
		return fmt.Errorf("deliverable %s needs a file or folder: %w", definition.Name, ErrConflict)
	}
	single := !bundle.Folder && bundle.Files == 1 && bundle.Entry != ""
	contentType := strings.ToLower(bundle.ContentType)
	switch kind {
	case domain.DeliverableKindFolder:
		if !bundle.Folder {
			return fmt.Errorf("deliverable %s is a folder: put a folder with at least one file: %w", definition.Name, ErrConflict)
		}
	case domain.DeliverableKindPDF:
		if !single || !strings.HasPrefix(contentType, "application/pdf") {
			return fmt.Errorf("deliverable %s is a PDF: put one .pdf file: %w", definition.Name, ErrConflict)
		}
	case domain.DeliverableKindImage:
		if !single || !strings.HasPrefix(contentType, "image/") {
			return fmt.Errorf("deliverable %s is an image: put one image file (png, jpg, gif, webp, svg): %w", definition.Name, ErrConflict)
		}
	case domain.DeliverableKindFile:
		if !single {
			return fmt.Errorf("deliverable %s is one file: put a single file: %w", definition.Name, ErrConflict)
		}
	}
	return nil
}

// Deliverable returns one stored revision by ID.
func (s *Store) Deliverable(deliverableID string) (domain.Deliverable, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	deliverable, ok := s.state.Deliverables[strings.TrimSpace(deliverableID)]
	if !ok {
		return domain.Deliverable{}, ErrNotFound
	}
	return deliverable, nil
}

// deliverableTargetLocked resolves the running phase of a Session and the
// deliverable it declares under name.
func (s *Store) deliverableTargetLocked(sessionID, name string) (domain.Job, domain.PhaseRun, domain.DeliverableDefinition, error) {
	session, ok := s.state.Sessions[sessionID]
	if !ok {
		return domain.Job{}, domain.PhaseRun{}, domain.DeliverableDefinition{}, ErrNotFound
	}
	job, _, run, phase, err := s.workflowLocked(session)
	if err != nil {
		return domain.Job{}, domain.PhaseRun{}, domain.DeliverableDefinition{}, err
	}
	if run.Status != domain.PhaseRunRunning {
		return domain.Job{}, domain.PhaseRun{}, domain.DeliverableDefinition{}, fmt.Errorf("phase is %s: %w", run.Status, ErrConflict)
	}
	for _, candidate := range phase.Deliverables {
		if strings.EqualFold(candidate.Name, name) {
			return job, run, candidate, nil
		}
	}
	return domain.Job{}, domain.PhaseRun{}, domain.DeliverableDefinition{}, fmt.Errorf("deliverable %q is not declared by phase %s: %w", name, phase.Name, ErrConflict)
}

func (s *Store) latestDeliverableLocked(jobID, name string) (domain.Deliverable, bool) {
	var latest domain.Deliverable
	found := false
	for _, existing := range s.state.Deliverables {
		if existing.JobID == jobID && strings.EqualFold(existing.Name, name) && (!found || existing.Revision > latest.Revision) {
			latest, found = existing, true
		}
	}
	return latest, found
}

// storeDeliverableLocked keeps exactly one revision per Session: a phase
// run that writes gets its own revision on the first write and keeps
// updating that same revision with every later rewrite or edit. A run that
// writes nothing leaves the previous revision as the latest. Comments
// re-anchor on their quoted text, so an edit after a comment is fine.
func (s *Store) storeDeliverableLocked(job domain.Job, run domain.PhaseRun, sessionID string, definition domain.DeliverableDefinition, content string, bundle *domain.DeliverableBundle) (domain.Deliverable, error) {
	revision := 1
	if latest, ok := s.latestDeliverableLocked(job.ID, definition.Name); ok {
		if latest.PhaseRunID == run.ID {
			latest.Content, latest.Bundle, latest.Kind = content, bundle, definition.Kind
			latest.UpdatedAt = time.Now().UTC()
			s.state.Deliverables[latest.ID] = latest
			return latest, s.saveLocked()
		}
		revision = latest.Revision + 1
	}
	deliverable := domain.Deliverable{
		ID: newID("del"), JobID: job.ID, PhaseRunID: run.ID, SessionID: sessionID,
		Name: definition.Name, Description: definition.Description, Content: content, Kind: definition.Kind, Bundle: bundle, Revision: revision, CreatedAt: time.Now().UTC(),
	}
	s.state.Deliverables[deliverable.ID] = deliverable
	return deliverable, s.saveLocked()
}

func (s *Store) SetWorkflowActionResult(sessionID string, result domain.WorkflowActionResult) (domain.PhaseRun, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	session, ok := s.state.Sessions[strings.TrimSpace(sessionID)]
	if !ok || session.PhaseRunID == "" {
		return domain.PhaseRun{}, ErrNotFound
	}
	_, _, run, phase, err := s.workflowLocked(session)
	if err != nil {
		return domain.PhaseRun{}, err
	}
	if phase.Executor != domain.WorkflowExecutorAction || phase.Action == nil || run.Status != domain.PhaseRunRunning {
		return domain.PhaseRun{}, fmt.Errorf("phase cannot record an action result: %w", ErrConflict)
	}
	result.Type = strings.TrimSpace(result.Type)
	result.ExternalID = strings.TrimSpace(result.ExternalID)
	result.URL = strings.TrimSpace(result.URL)
	result.Detail = strings.TrimSpace(result.Detail)
	// A pull request is its URL; a merge is a commit, which has a URL only
	// on providers whose layout is known.
	if result.Type != phase.Action.Type || (result.URL == "" && result.Type != domain.WorkflowActionGitMerge) {
		return domain.PhaseRun{}, fmt.Errorf("action type and result URL are required: %w", ErrConflict)
	}
	if result.CreatedAt.IsZero() {
		result.CreatedAt = time.Now().UTC()
	}
	run.ActionResult = &result
	s.state.PhaseRuns[run.ID] = run
	return run, s.saveLocked()
}

func (s *Store) AddDeliverableComment(deliverableID, author string, req domain.CreateDeliverableCommentRequest) (domain.DeliverableComment, error) {
	author = normalizeSubject(author)
	body := strings.TrimSpace(req.Body)
	if author == "" || body == "" {
		return domain.DeliverableComment{}, fmt.Errorf("author and comment are required: %w", ErrConflict)
	}
	if len(req.SelectedText) > 16<<10 || len(req.Prefix) > 512 || len(req.Suffix) > 512 || len(body) > 8<<10 {
		return domain.DeliverableComment{}, fmt.Errorf("comment selection or body is too large: %w", ErrConflict)
	}

	s.mu.Lock()
	defer s.mu.Unlock()
	deliverable, ok := s.state.Deliverables[strings.TrimSpace(deliverableID)]
	if !ok {
		return domain.DeliverableComment{}, ErrNotFound
	}
	// A document comment anchors on selected text; a comment on any other
	// kind is about the whole revision.
	if domain.DeliverableIsBundle(deliverable.Kind) {
		req.SelectedText, req.StartOffset, req.EndOffset, req.Prefix, req.Suffix = "", 0, 0, "", ""
	} else if strings.TrimSpace(req.SelectedText) == "" || req.StartOffset < 0 || req.EndOffset <= req.StartOffset {
		return domain.DeliverableComment{}, fmt.Errorf("a comment on a document needs selected text with valid offsets: %w", ErrConflict)
	}
	latest := deliverable
	for _, candidate := range s.state.Deliverables {
		if candidate.JobID == deliverable.JobID && strings.EqualFold(candidate.Name, deliverable.Name) && candidate.Revision > latest.Revision {
			latest = candidate
		}
	}
	if latest.ID != deliverable.ID {
		return domain.DeliverableComment{}, fmt.Errorf("deliverable revision %d is historical; revision %d is current: %w", deliverable.Revision, latest.Revision, ErrConflict)
	}
	comment := domain.DeliverableComment{
		ID: newID("com"), DeliverableID: deliverable.ID,
		SelectedText: req.SelectedText, StartOffset: req.StartOffset, EndOffset: req.EndOffset,
		Prefix: req.Prefix, Suffix: req.Suffix, Body: body, Author: author, CreatedAt: time.Now().UTC(),
	}
	s.state.DeliverableComments[comment.ID] = comment
	return comment, s.saveLocked()
}

const (
	maxWorkflowQuestionItems   = 6
	maxWorkflowQuestionOptions = 8
)

// AskWorkflowQuestion asks one open question; see AskWorkflowQuestions.
func (s *Store) AskWorkflowQuestion(sessionID, question string) (domain.WorkflowQuestion, error) {
	return s.AskWorkflowQuestions(sessionID, []domain.WorkflowQuestionItem{{Question: question}})
}

// AskWorkflowQuestions pauses the phase with one form of questions. Each item
// may carry the answers the agent expects; the operator can always answer in
// their own words, so an "other" option never has to be spelled out.
func (s *Store) AskWorkflowQuestions(sessionID string, items []domain.WorkflowQuestionItem) (domain.WorkflowQuestion, error) {
	if len(items) == 0 || len(items) > maxWorkflowQuestionItems {
		return domain.WorkflowQuestion{}, fmt.Errorf("between 1 and %d questions are required: %w", maxWorkflowQuestionItems, ErrConflict)
	}
	cleaned := make([]domain.WorkflowQuestionItem, 0, len(items))
	headlines := make([]string, 0, len(items))
	for index, item := range items {
		text := strings.TrimSpace(item.Question)
		if text == "" || len(text) > 4000 {
			return domain.WorkflowQuestion{}, fmt.Errorf("question %d must contain 1 to 4000 characters: %w", index+1, ErrConflict)
		}
		if len(item.Options) > maxWorkflowQuestionOptions {
			return domain.WorkflowQuestion{}, fmt.Errorf("question %d offers more than %d options: %w", index+1, maxWorkflowQuestionOptions, ErrConflict)
		}
		options := make([]string, 0, len(item.Options))
		for _, option := range item.Options {
			option = strings.TrimSpace(option)
			if option == "" || len(option) > 400 {
				return domain.WorkflowQuestion{}, fmt.Errorf("question %d has an option outside 1 to 400 characters: %w", index+1, ErrConflict)
			}
			if !slices.Contains(options, option) {
				options = append(options, option)
			}
		}
		cleaned = append(cleaned, domain.WorkflowQuestionItem{ID: fmt.Sprintf("q%d", index+1), Question: text, Options: options})
		headlines = append(headlines, text)
	}
	question := strings.Join(headlines, " · ")
	s.mu.Lock()
	defer s.mu.Unlock()
	session, ok := s.state.Sessions[sessionID]
	if !ok {
		return domain.WorkflowQuestion{}, ErrNotFound
	}
	job, template, run, phase, err := s.workflowLocked(session)
	if err != nil {
		return domain.WorkflowQuestion{}, err
	}
	if run.Status != domain.PhaseRunRunning {
		return domain.WorkflowQuestion{}, fmt.Errorf("phase is %s: %w", run.Status, ErrConflict)
	}
	now := time.Now().UTC()
	s.supersedeOpenQuestionsLocked(run.ID, now)
	created := domain.WorkflowQuestion{
		ID: newID("ask"), JobID: job.ID, PhaseRunID: run.ID, SessionID: session.ID,
		Kind: "agent", Question: question, Items: cleaned, Outcome: "ask",
		AcceptTarget: humanWorkflowTarget(template, phase.ID, phase.Accept.Target, domain.WorkflowTargetNext),
		RejectTarget: humanWorkflowTarget(template, phase.ID, phase.Reject.Target, domain.WorkflowTargetSelf),
		Status:       "open", CreatedAt: now,
	}
	run.Status = domain.PhaseRunPending
	run.PendingReason = "ask"
	run.PendingOutcome = "ask"
	job.WorkflowStatus = domain.WorkflowPending
	job.PendingReason = "ask"
	job.UpdatedAt = now
	s.state.WorkflowQuestions[created.ID] = created
	s.state.PhaseRuns[run.ID] = run
	s.state.Jobs[job.ID] = job
	return created, s.saveLocked()
}

func (s *Store) CompleteWorkflowPhase(sessionID, outcome, detail string) (domain.WorkflowAdvance, error) {
	outcome = strings.ToLower(strings.TrimSpace(outcome))
	detail = strings.TrimSpace(detail)
	if outcome != "accept" && outcome != "reject" {
		return domain.WorkflowAdvance{}, fmt.Errorf("outcome must be accept or reject: %w", ErrConflict)
	}
	if outcome == "reject" && detail == "" {
		return domain.WorkflowAdvance{}, fmt.Errorf("reject requires a reason: %w", ErrConflict)
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	session, ok := s.state.Sessions[sessionID]
	if !ok {
		return domain.WorkflowAdvance{}, ErrNotFound
	}
	job, template, run, phase, err := s.workflowLocked(session)
	if err != nil {
		return domain.WorkflowAdvance{}, err
	}
	if run.Status != domain.PhaseRunRunning {
		return domain.WorkflowAdvance{}, fmt.Errorf("phase is %s: %w", run.Status, ErrConflict)
	}
	transition := phase.Accept
	rejectionCount := 0
	if outcome == "reject" {
		transition = phase.Reject
		rejectionCount = s.rejectionCountLocked(job.ID, phase.ID) + 1
		if transition.Max > 0 && rejectionCount >= transition.Max {
			transition.Target = transition.Exhausted
			if transition.Target == "" {
				transition.Target = domain.WorkflowTargetAskUser
			}
		}
	}
	// An expose phase exists to be judged by a person: it always waits.
	needsUser := transition.AskUser || phase.Executor == domain.WorkflowExecutorExpose || resolveWorkflowTarget(template, phase.ID, transition.Target) == domain.WorkflowTargetAskUser
	if !needsUser {
		if err := s.validateWorkflowInjectionLocked(job.ID, template, phase.ID, transition.Target); err != nil {
			return domain.WorkflowAdvance{}, err
		}
	}
	if outcome == "accept" {
		for _, required := range phase.Deliverables {
			if !required.Required || s.hasDeliverableLocked(run.ID, required.Name) {
				continue
			}
			return domain.WorkflowAdvance{}, fmt.Errorf("required deliverable %s is missing: %w", required.Name, ErrConflict)
		}
	}
	now := time.Now().UTC()
	s.supersedeOpenQuestionsLocked(run.ID, now)
	run.AgentOutcomes = append(run.AgentOutcomes, domain.WorkflowAgentOutcome{
		ID: newID("out"), Outcome: outcome, Detail: detail, CreatedAt: now,
	})
	if outcome == "accept" {
		run.Status = domain.PhaseRunAccepted
		run.Summary = detail
	} else {
		run.Status = domain.PhaseRunRejected
		run.RejectReason = detail
	}
	run.CompletedAt = &now
	s.state.PhaseRuns[run.ID] = run
	if needsUser {
		advance, err := s.awaitWorkflowDecisionLocked(job, template, run, phase, outcome, rejectionCount)
		if err != nil {
			return domain.WorkflowAdvance{}, err
		}
		if err := s.saveLocked(); err != nil {
			return domain.WorkflowAdvance{}, err
		}
		return advance, nil
	}
	advance, err := s.advanceWorkflowLocked(job, template, run, phase, transition.Target, outcome)
	if err != nil {
		return domain.WorkflowAdvance{}, err
	}
	if err := s.saveLocked(); err != nil {
		return domain.WorkflowAdvance{}, err
	}
	return advance, nil
}

func (s *Store) hasDeliverableLocked(phaseRunID, name string) bool {
	for _, deliverable := range s.state.Deliverables {
		if deliverable.PhaseRunID == phaseRunID && strings.EqualFold(deliverable.Name, name) {
			return true
		}
	}
	return false
}

func (s *Store) validateWorkflowInjectionLocked(jobID string, template domain.WorkflowTemplate, currentPhaseID, rawTarget string) error {
	target := resolveWorkflowTarget(template, currentPhaseID, rawTarget)
	if target == domain.WorkflowTargetDone || target == domain.WorkflowTargetAskUser {
		return nil
	}
	phase, ok := workflowPhase(template, target)
	if !ok {
		return fmt.Errorf("workflow target %q: %w", target, ErrConflict)
	}
	for _, name := range phase.Inject {
		available := false
		for _, deliverable := range s.state.Deliverables {
			if deliverable.JobID == jobID && strings.EqualFold(deliverable.Name, name) {
				available = true
				break
			}
		}
		if !available {
			return fmt.Errorf("phase %s requires injected deliverable %s, but no revision exists yet: %w", phase.Name, name, ErrConflict)
		}
	}
	return nil
}

// ValidateWorkflowPhaseTransition checks the input contract of the phase that
// would be entered immediately. A gated outcome has no immediate transition;
// its concrete human decision is validated separately.
func (s *Store) ValidateWorkflowPhaseTransition(sessionID, outcome string) error {
	outcome = strings.ToLower(strings.TrimSpace(outcome))
	if outcome != "accept" && outcome != "reject" {
		return fmt.Errorf("outcome must be accept or reject: %w", ErrConflict)
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	session, ok := s.state.Sessions[sessionID]
	if !ok {
		return ErrNotFound
	}
	job, template, _, phase, err := s.workflowLocked(session)
	if err != nil {
		return err
	}
	transition := phase.Accept
	if outcome == "reject" {
		transition = phase.Reject
		rejectionCount := s.rejectionCountLocked(job.ID, phase.ID) + 1
		if transition.Max > 0 && rejectionCount >= transition.Max {
			transition.Target = transition.Exhausted
			if transition.Target == "" {
				transition.Target = domain.WorkflowTargetAskUser
			}
		}
	}
	if transition.AskUser || resolveWorkflowTarget(template, phase.ID, transition.Target) == domain.WorkflowTargetAskUser {
		return nil
	}
	return s.validateWorkflowInjectionLocked(job.ID, template, phase.ID, transition.Target)
}

func (s *Store) ValidateWorkflowQuestionTransition(questionID, action string) error {
	action = strings.ToLower(strings.TrimSpace(action))
	if action != "accept" && action != "reject" {
		return fmt.Errorf("action must be accept or reject: %w", ErrConflict)
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	question, ok := s.state.WorkflowQuestions[questionID]
	if !ok || question.Status != "open" {
		return ErrNotFound
	}
	job, ok := s.state.Jobs[question.JobID]
	if !ok {
		return ErrNotFound
	}
	template, ok := s.workflowTemplateForJobLocked(job)
	if !ok {
		return ErrNotFound
	}
	run, ok := s.state.PhaseRuns[question.PhaseRunID]
	if !ok {
		return ErrNotFound
	}
	phase, ok := workflowPhase(template, run.PhaseID)
	if !ok {
		return ErrNotFound
	}
	target := question.AcceptTarget
	if action == "reject" {
		target = question.RejectTarget
	}
	if target == "" {
		if action == "accept" {
			target = humanWorkflowTarget(template, phase.ID, phase.Accept.Target, domain.WorkflowTargetNext)
		} else {
			target = humanWorkflowTarget(template, phase.ID, phase.Reject.Target, domain.WorkflowTargetSelf)
		}
	}
	return s.validateWorkflowInjectionLocked(job.ID, template, phase.ID, target)
}

func (s *Store) rejectionCountLocked(jobID, phaseID string) int {
	count := 0
	for _, run := range s.state.PhaseRuns {
		if run.JobID != jobID || run.PhaseID != phaseID {
			continue
		}
		if len(run.AgentOutcomes) == 0 {
			if run.Status == domain.PhaseRunRejected {
				count++
			}
			continue
		}
		for _, outcome := range run.AgentOutcomes {
			if outcome.Outcome == "reject" {
				count++
			}
		}
	}
	return count
}

func (s *Store) advanceWorkflowLocked(job domain.Job, template domain.WorkflowTemplate, run domain.PhaseRun, phase domain.WorkflowPhase, rawTarget, outcome string) (domain.WorkflowAdvance, error) {
	target := resolveWorkflowTarget(template, phase.ID, rawTarget)
	advance := domain.WorkflowAdvance{Job: job, PhaseRun: run}
	if target == domain.WorkflowTargetAskUser {
		return s.awaitWorkflowDecisionLocked(job, template, run, phase, outcome, s.rejectionCountLocked(job.ID, phase.ID))
	}
	if target == domain.WorkflowTargetDone {
		job.Status = domain.JobDone
		job.WorkflowStatus = domain.WorkflowDone
		job.PendingReason = ""
		job.CurrentPhaseRunID = ""
		job.UpdatedAt = time.Now().UTC()
		s.state.Jobs[job.ID] = job
		advance.Job = job
		return advance, nil
	}
	nextPhase, ok := workflowPhase(template, target)
	if !ok {
		return domain.WorkflowAdvance{}, fmt.Errorf("workflow target %q: %w", target, ErrConflict)
	}
	nextSession, nextRun := s.newWorkflowSessionLocked(&job, template, nextPhase, run.SessionID)
	s.state.Sessions[nextSession.ID] = nextSession
	s.state.PhaseRuns[nextRun.ID] = nextRun
	s.state.Jobs[job.ID] = job
	advance.Job, advance.PhaseRun, advance.NextSession = job, nextRun, &nextSession
	return advance, nil
}

func humanWorkflowTarget(template domain.WorkflowTemplate, phaseID, rawTarget, fallback string) string {
	target := resolveWorkflowTarget(template, phaseID, rawTarget)
	if target == domain.WorkflowTargetAskUser {
		return resolveWorkflowTarget(template, phaseID, fallback)
	}
	return target
}

func (s *Store) awaitWorkflowDecisionLocked(job domain.Job, template domain.WorkflowTemplate, run domain.PhaseRun, phase domain.WorkflowPhase, outcome string, rejectionCount int) (domain.WorkflowAdvance, error) {
	now := time.Now().UTC()
	questionText := fmt.Sprintf("AI accepted %s.", phase.Name)
	if phase.Executor == domain.WorkflowExecutorExpose {
		questionText = fmt.Sprintf("De test-app van %s staat klaar.", phase.Name)
	}
	if run.Summary != "" {
		questionText += " " + run.Summary
	}
	if outcome == "reject" {
		if rejectionCount < 1 {
			rejectionCount = 1
		}
		questionText = fmt.Sprintf("AI rejected %s %d keer. %s", phase.Name, rejectionCount, run.RejectReason)
	}
	questionKind := "approval"
	acceptTarget := humanWorkflowTarget(template, phase.ID, phase.Accept.Target, domain.WorkflowTargetNext)
	rejectTarget := humanWorkflowTarget(template, phase.ID, phase.Reject.Target, domain.WorkflowTargetSelf)
	if phase.Executor == domain.WorkflowExecutorAction && outcome == "reject" {
		// A failed merge or pull request cannot be approved away: either the
		// person retries it or the Job stays pending without a false DONE.
		questionKind = "action"
		acceptTarget = phase.ID
		rejectTarget = phase.ID
	}
	question := domain.WorkflowQuestion{
		ID: newID("ask"), JobID: job.ID, PhaseRunID: run.ID, SessionID: run.SessionID,
		Kind: questionKind, Question: strings.TrimSpace(questionText), Outcome: outcome,
		AgentDetail:  map[string]string{"accept": run.Summary, "reject": run.RejectReason}[outcome],
		AcceptTarget: acceptTarget,
		RejectTarget: rejectTarget,
		Status:       "open", CreatedAt: now,
	}
	if len(run.AgentOutcomes) > 0 {
		question.AgentOutcomeID = run.AgentOutcomes[len(run.AgentOutcomes)-1].ID
	}
	run.Status = domain.PhaseRunPending
	run.PendingReason = "user"
	run.PendingOutcome = outcome
	run.CompletedAt = nil
	job.WorkflowStatus = domain.WorkflowPending
	job.PendingReason = "user"
	job.UpdatedAt = now
	s.state.WorkflowQuestions[question.ID] = question
	s.state.PhaseRuns[run.ID] = run
	s.state.Jobs[job.ID] = job
	return domain.WorkflowAdvance{Job: job, PhaseRun: run, Question: &question}, nil
}

func resolveWorkflowTarget(template domain.WorkflowTemplate, currentPhaseID, raw string) string {
	target := strings.TrimSpace(raw)
	switch strings.ToUpper(target) {
	case "", domain.WorkflowTargetNext:
		for index, phase := range template.Phases {
			if phase.ID == currentPhaseID {
				if index+1 < len(template.Phases) {
					return template.Phases[index+1].ID
				}
				return domain.WorkflowTargetDone
			}
		}
		return domain.WorkflowTargetDone
	case domain.WorkflowTargetSelf:
		return currentPhaseID
	case domain.WorkflowTargetDone:
		return domain.WorkflowTargetDone
	case domain.WorkflowTargetAskUser:
		return domain.WorkflowTargetAskUser
	default:
		return normalizeName(target)
	}
}

func (s *Store) AnswerWorkflowQuestion(questionID, operator, action, reason string) (domain.WorkflowAdvance, error) {
	operator = normalizeSubject(operator)
	action = strings.ToLower(strings.TrimSpace(action))
	reason = strings.TrimSpace(reason)
	if operator == "" || (action != "accept" && action != "reject") {
		return domain.WorkflowAdvance{}, fmt.Errorf("operator and action accept/reject are required: %w", ErrConflict)
	}
	if action == "reject" && reason == "" {
		return domain.WorkflowAdvance{}, fmt.Errorf("reject always requires a reason: %w", ErrConflict)
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	question, ok := s.state.WorkflowQuestions[questionID]
	if !ok || question.Status != "open" {
		return domain.WorkflowAdvance{}, ErrNotFound
	}
	run, ok := s.state.PhaseRuns[question.PhaseRunID]
	if !ok || run.Status != domain.PhaseRunPending {
		return domain.WorkflowAdvance{}, ErrConflict
	}
	job, ok := s.state.Jobs[question.JobID]
	if !ok {
		return domain.WorkflowAdvance{}, ErrNotFound
	}
	template, ok := s.workflowTemplateForJobLocked(job)
	if !ok {
		return domain.WorkflowAdvance{}, ErrNotFound
	}
	phase, ok := workflowPhase(template, run.PhaseID)
	if !ok {
		return domain.WorkflowAdvance{}, ErrNotFound
	}
	if action == "accept" {
		for _, required := range phase.Deliverables {
			if !required.Required || s.hasDeliverableLocked(run.ID, required.Name) {
				continue
			}
			return domain.WorkflowAdvance{}, fmt.Errorf("required deliverable %s is missing: %w", required.Name, ErrConflict)
		}
	}
	now := time.Now().UTC()
	question.Answer = action
	question.Reason = reason
	question.AnsweredBy = operator
	question.Status = "answered"
	question.AnsweredAt = &now
	target := question.AcceptTarget
	if target == "" {
		target = humanWorkflowTarget(template, phase.ID, phase.Accept.Target, domain.WorkflowTargetNext)
	}
	run.Status = domain.PhaseRunAccepted
	if run.Summary == "" {
		run.Summary = "Accepted by user"
	}
	if action == "reject" {
		target = question.RejectTarget
		if target == "" {
			target = humanWorkflowTarget(template, phase.ID, phase.Reject.Target, domain.WorkflowTargetSelf)
		}
	}
	if err := s.validateWorkflowInjectionLocked(job.ID, template, phase.ID, target); err != nil {
		return domain.WorkflowAdvance{}, err
	}
	if action == "reject" {
		run.Status = domain.PhaseRunRejected
		run.RejectReason = reason
	}
	s.state.WorkflowQuestions[question.ID] = question
	run.PendingReason = ""
	run.PendingOutcome = ""
	run.CompletedAt = &now
	s.state.PhaseRuns[run.ID] = run
	advance, err := s.advanceWorkflowLocked(job, template, run, phase, target, action)
	if err != nil {
		return domain.WorkflowAdvance{}, err
	}
	advance.Question = &question
	if err := s.saveLocked(); err != nil {
		return domain.WorkflowAdvance{}, err
	}
	return advance, nil
}

// AnswerWorkflowQuestions records the operator's answer to every item of an
// agent ask and hands the phase back to the same Session. Unlike ACCEPT and
// REJECT it routes nowhere: the agent asked because it needed input to carry on,
// not because the phase was finished. An answer that is not one of the offered
// options is kept verbatim and marked as the operator's own words.
func (s *Store) AnswerWorkflowQuestions(questionID, operator string, answers []domain.WorkflowQuestionAnswer) (domain.WorkflowQuestion, error) {
	operator = normalizeSubject(operator)
	if operator == "" {
		return domain.WorkflowQuestion{}, fmt.Errorf("operator is required: %w", ErrConflict)
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	question, ok := s.state.WorkflowQuestions[questionID]
	if !ok || question.Status != "open" {
		return domain.WorkflowQuestion{}, ErrNotFound
	}
	if question.Kind != "agent" || len(question.Items) == 0 {
		return domain.WorkflowQuestion{}, fmt.Errorf("this question has no answer form; use accept, reject or chat: %w", ErrConflict)
	}
	run, ok := s.state.PhaseRuns[question.PhaseRunID]
	if !ok || run.Status != domain.PhaseRunPending {
		return domain.WorkflowQuestion{}, ErrConflict
	}
	job, ok := s.state.Jobs[run.JobID]
	if !ok {
		return domain.WorkflowQuestion{}, ErrNotFound
	}
	given := make(map[string]string, len(answers))
	for _, answer := range answers {
		given[strings.TrimSpace(answer.ItemID)] = strings.TrimSpace(answer.Answer)
	}
	items := make([]domain.WorkflowQuestionItem, len(question.Items))
	for index, item := range question.Items {
		answer, ok := given[item.ID]
		if !ok || answer == "" {
			return domain.WorkflowQuestion{}, fmt.Errorf("question %d (%s) has no answer: %w", index+1, item.Question, ErrConflict)
		}
		if len(answer) > 4000 {
			return domain.WorkflowQuestion{}, fmt.Errorf("answer %d exceeds 4000 characters: %w", index+1, ErrConflict)
		}
		item.Answer = answer
		item.Other = !slices.Contains(item.Options, answer)
		items[index] = item
	}
	now := time.Now().UTC()
	question.Items = items
	question.Answer = "answered"
	question.Reason = ""
	question.AnsweredBy = operator
	question.Status = "answered"
	question.AnsweredAt = &now
	run.Status = domain.PhaseRunRunning
	run.PendingReason = ""
	run.PendingOutcome = ""
	run.CompletedAt = nil
	job.WorkflowStatus = domain.WorkflowBusy
	job.PendingReason = ""
	job.UpdatedAt = now
	s.state.WorkflowQuestions[question.ID] = question
	s.state.PhaseRuns[run.ID] = run
	s.state.Jobs[job.ID] = job
	return question, s.saveLocked()
}

// ResumeWorkflowPhaseForChat lets the same ACP Session work again while the
// pending decision stays open. A chat is a way to ask the agent something, not
// a verdict on the phase: the operator may still ACCEPT or REJECT the standing
// outcome afterwards, and only a new accept or reject from the agent replaces
// it. While the agent works the run is running, which keeps the human buttons
// closed; SettleWorkflowChatTurn reopens them when the turn ends.
func (s *Store) ResumeWorkflowPhaseForChat(sessionID, operator string) (bool, error) {
	operator = normalizeSubject(operator)
	s.mu.Lock()
	defer s.mu.Unlock()
	session, ok := s.state.Sessions[sessionID]
	if !ok || session.PhaseRunID == "" {
		return false, nil
	}
	if operator == "" || session.Operator != operator {
		return false, ErrConflict
	}
	run, ok := s.state.PhaseRuns[session.PhaseRunID]
	if !ok || run.Status != domain.PhaseRunPending {
		return false, nil
	}
	if _, found := s.openQuestionLocked(run.ID); !found {
		return false, nil
	}
	job, ok := s.state.Jobs[run.JobID]
	if !ok {
		return false, ErrNotFound
	}
	now := time.Now().UTC()
	run.Status = domain.PhaseRunRunning
	run.PendingReason = ""
	run.CompletedAt = nil
	job.WorkflowStatus = domain.WorkflowBusy
	job.PendingReason = ""
	job.UpdatedAt = now
	s.state.PhaseRuns[run.ID] = run
	s.state.Jobs[job.ID] = job
	return true, s.saveLocked()
}

// SettleWorkflowChatTurn runs when the agent ends a turn. A phase that was
// resumed for a chat and still carries an open decision goes back to pending on
// that decision, so the operator's ACCEPT and REJECT are clickable again. A
// turn that produced a new decision, or a turn outside a chat, changes nothing.
//
// A run that is running without any open decision but with a standing agent
// outcome is the trace of an older build, where a chat closed the decision as
// answered. The agent has now ended its turn without deciding again, so that
// outcome is put back in front of the operator instead of leaving the phase
// running with nothing to click.
func (s *Store) SettleWorkflowChatTurn(sessionID string) (bool, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	session, ok := s.state.Sessions[sessionID]
	if !ok || session.PhaseRunID == "" {
		return false, nil
	}
	run, ok := s.state.PhaseRuns[session.PhaseRunID]
	if !ok || run.Status != domain.PhaseRunRunning {
		return false, nil
	}
	question, found := s.openQuestionLocked(run.ID)
	if !found {
		return s.restoreStandingDecisionLocked(session, run)
	}
	job, ok := s.state.Jobs[run.JobID]
	if !ok {
		return false, ErrNotFound
	}
	reason := "user"
	if question.Kind == "agent" {
		reason = "ask"
	}
	now := time.Now().UTC()
	run.Status = domain.PhaseRunPending
	run.PendingReason = reason
	run.PendingOutcome = question.Outcome
	job.WorkflowStatus = domain.WorkflowPending
	job.PendingReason = reason
	job.UpdatedAt = now
	s.state.PhaseRuns[run.ID] = run
	s.state.Jobs[job.ID] = job
	return true, s.saveLocked()
}

// RepairStandingDecisions puts back every decision that a chat under an older
// build closed while the phase kept running. It runs when the server starts:
// that is the one moment no agent is working on anything, so a running run
// with a standing outcome and no open decision can only be that leftover.
func (s *Store) RepairStandingDecisions() (int, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	restored := 0
	for _, session := range s.state.Sessions {
		if session.PhaseRunID == "" {
			continue
		}
		run, ok := s.state.PhaseRuns[session.PhaseRunID]
		if !ok || run.Status != domain.PhaseRunRunning || len(run.AgentOutcomes) == 0 {
			continue
		}
		if _, open := s.openQuestionLocked(run.ID); open {
			continue
		}
		done, err := s.restoreStandingDecisionLocked(session, run)
		if err != nil {
			return restored, err
		}
		if done {
			restored++
		}
	}
	return restored, nil
}

func (s *Store) restoreStandingDecisionLocked(session domain.Session, run domain.PhaseRun) (bool, error) {
	if len(run.AgentOutcomes) == 0 {
		return false, nil
	}
	job, template, run, phase, err := s.workflowLocked(session)
	if err != nil {
		return false, err
	}
	last := run.AgentOutcomes[len(run.AgentOutcomes)-1]
	rejectionCount := 0
	if last.Outcome == "reject" {
		rejectionCount = s.rejectionCountLocked(job.ID, phase.ID)
	}
	if _, err := s.awaitWorkflowDecisionLocked(job, template, run, phase, last.Outcome, rejectionCount); err != nil {
		return false, err
	}
	return true, s.saveLocked()
}

func (s *Store) openQuestionLocked(runID string) (domain.WorkflowQuestion, bool) {
	for _, candidate := range s.state.WorkflowQuestions {
		if candidate.PhaseRunID == runID && candidate.Status == "open" {
			return candidate, true
		}
	}
	return domain.WorkflowQuestion{}, false
}

// supersedeOpenQuestionsLocked closes the standing decision when the agent
// takes a new one during a chat. Nobody answered it, so it is neither open nor
// answered; the agent outcome it belonged to stays in the run's history.
func (s *Store) supersedeOpenQuestionsLocked(runID string, now time.Time) {
	for id, candidate := range s.state.WorkflowQuestions {
		if candidate.PhaseRunID != runID || candidate.Status != "open" {
			continue
		}
		candidate.Status = "superseded"
		candidate.AnsweredAt = &now
		s.state.WorkflowQuestions[id] = candidate
	}
}

func (s *Store) newWorkflowSessionLocked(job *domain.Job, template domain.WorkflowTemplate, phase domain.WorkflowPhase, parentSessionID string) (domain.Session, domain.PhaseRun) {
	now := time.Now().UTC()
	sessionID := newID("ses")
	runID := newID("run")
	attempt := 1
	for _, existing := range s.state.PhaseRuns {
		if existing.JobID == job.ID && existing.PhaseID == phase.ID && existing.Attempt >= attempt {
			attempt = existing.Attempt + 1
		}
	}
	// The phase runs as whoever the Job is with now: handing a Job over
	// takes effect from the next phase, never in the middle of one.
	worker := job.Worker()
	environmentSelector, withSelectors := s.phaseEnvironmentLocked(worker, phase, job.EnvironmentSelector, job.WithSelectors)
	tool := s.stackAgentToolLocked(worker, environmentSelector, withSelectors)
	// A pull request is control-plane API work and needs no workspace; a
	// merge runs git in a workspace of the Job's environment.
	if phase.Executor == domain.WorkflowExecutorAction && (phase.Action == nil || phase.Action.Type != domain.WorkflowActionGitMerge) {
		environmentSelector, withSelectors, tool = "", nil, ""
	}
	namespace := strings.TrimSuffix(job.Branch, "/main")
	session := domain.Session{
		ID: sessionID, JobID: job.ID, PhaseRunID: runID, ParentSessionID: parentSessionID,
		SpawnedBySessionID: parentSessionID, ForkMode: domain.ForkRoot, Tool: tool,
		Executor: phase.Executor, EnvironmentSelector: environmentSelector, WithSelectors: withSelectors,
		MCPServerIDs: append([]string{}, job.MCPServerIDs...), Role: phase.Name, Model: job.Model,
		Operator: worker, ObjectiveDelta: phase.Instructions, GitRepositoryID: job.GitRepositoryID,
		BaseRef: job.Branch, GitRef: namespace + "/sessions/" + sessionID, TargetBranch: job.Branch,
		Status: domain.SessionQueued, TurnIDs: []string{}, CheckpointIDs: []string{},
		ContinuityLevel: "workflow_phase", ContinuityScore: 10, CreatedAt: now, UpdatedAt: now,
	}
	run := domain.PhaseRun{
		ID: runID, JobID: job.ID, TemplateID: template.ID, PhaseID: phase.ID, PhaseName: phase.Name,
		Attempt: attempt, SessionID: sessionID, Status: domain.PhaseRunQueued, StartedAt: now,
	}
	job.SessionIDs = append(job.SessionIDs, sessionID)
	job.PhaseRunIDs = append(job.PhaseRunIDs, runID)
	job.CurrentPhaseRunID = runID
	job.Status = domain.JobActive
	job.WorkflowStatus = domain.WorkflowBusy
	job.PendingReason = ""
	job.UpdatedAt = now
	return session, run
}

// AdoptWorkflowTemplate moves an active Job to the newest revision of its
// Template, continuing at the chosen step of that revision as a new
// attempt: the current step is closed as "overgezet", so the Job never
// sits in a step the new revision does not know. It reports the
// composition the closed step was using, so the caller can stop it.
func (s *Store) AdoptWorkflowTemplate(jobID, operator, phaseID string) (domain.CreateJobResponse, string, bool, error) {
	operator = normalizeSubject(operator)
	phaseID = normalizeName(phaseID)
	s.mu.Lock()
	defer s.mu.Unlock()
	job, ok := s.state.Jobs[strings.TrimSpace(jobID)]
	if !ok {
		return domain.CreateJobResponse{}, "", false, ErrNotFound
	}
	if !job.AllowsOperator(operator) {
		return domain.CreateJobResponse{}, "", false, fmt.Errorf("only the owner or assignee of the Job can move it to a newer Template: %w", ErrConflict)
	}
	if job.Status == domain.JobDone || job.Status == domain.JobCancelled || job.TemplateID == "" {
		return domain.CreateJobResponse{}, "", false, fmt.Errorf("only an active workflow Job can move to a newer Template: %w", ErrConflict)
	}
	latest, ok := s.state.WorkflowTemplates[job.TemplateID]
	if !ok {
		return domain.CreateJobResponse{}, "", false, fmt.Errorf("the Job's Template no longer exists: %w", ErrNotFound)
	}
	template := cloneWorkflowTemplate(latest)
	run, hasRun := s.state.PhaseRuns[job.CurrentPhaseRunID]
	if phaseID == "" {
		return domain.CreateJobResponse{}, "", false, fmt.Errorf("choose the step of revision %d to continue at: %w", template.Revision, ErrConflict)
	}
	phase, exists := workflowPhase(template, phaseID)
	if !exists {
		return domain.CreateJobResponse{}, "", false, fmt.Errorf("revision %d has no step %q: %w", template.Revision, phaseID, ErrConflict)
	}
	now := time.Now().UTC()
	previousCompositionID := ""
	if hasRun {
		for id, question := range s.state.WorkflowQuestions {
			if question.PhaseRunID != run.ID || question.Status != "open" {
				continue
			}
			question.Status = "answered"
			question.Answer = "retry"
			question.Reason = "Job overgezet naar stap " + phase.Name + " (Template r" + strconv.Itoa(template.Revision) + ")"
			question.AnsweredBy = operator
			question.AnsweredAt = &now
			s.state.WorkflowQuestions[id] = question
		}
		switch run.Status {
		case domain.PhaseRunQueued, domain.PhaseRunRunning, domain.PhaseRunPending:
			// Closed without a verdict: it is neither feedback for the next
			// step nor a success, just where the Job left the old revision.
			run.Status = domain.PhaseRunAccepted
			run.Summary = "Overgezet naar stap " + phase.Name + " (Template r" + strconv.Itoa(template.Revision) + ") door " + operator
			run.PendingReason, run.PendingOutcome = "", ""
			run.CompletedAt = &now
			s.state.PhaseRuns[run.ID] = run
		}
		if session, ok := s.state.Sessions[run.SessionID]; ok {
			previousCompositionID = session.PreparedCompositionID
			if session.Status == domain.SessionQueued || session.Status == domain.SessionRunning {
				session.Status = domain.SessionCompleted
				session.UpdatedAt = now
				s.state.Sessions[session.ID] = session
			}
		}
	}
	job.TemplateSnapshot = &template
	parent := ""
	if hasRun {
		parent = run.SessionID
	}
	session, nextRun := s.newWorkflowSessionLocked(&job, template, phase, parent)
	s.state.Sessions[session.ID] = session
	s.state.PhaseRuns[nextRun.ID] = nextRun
	s.state.Jobs[job.ID] = job
	if err := s.saveLocked(); err != nil {
		return domain.CreateJobResponse{}, "", false, err
	}
	return domain.CreateJobResponse{Job: job, Session: session}, previousCompositionID, true, nil
}

// StartProcess ends a Job's brainstorm with the goal the chat arrived at:
// the goal goes on the Job, the brainstorm run is accepted with it as its
// summary, and the Template's first step is queued. It reports the
// composition the brainstorm was using, so the caller can let it go.
func (s *Store) StartProcess(sessionID, goal string) (domain.CreateJobResponse, string, error) {
	goal = strings.TrimSpace(goal)
	if goal == "" {
		return domain.CreateJobResponse{}, "", fmt.Errorf("a goal is required to start the process: %w", ErrConflict)
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	session, ok := s.state.Sessions[strings.TrimSpace(sessionID)]
	if !ok || session.PhaseRunID == "" {
		return domain.CreateJobResponse{}, "", ErrNotFound
	}
	job, template, run, _, err := s.workflowLocked(session)
	if err != nil {
		return domain.CreateJobResponse{}, "", err
	}
	if run.PhaseID != domain.BrainstormPhaseID || run.Status != domain.PhaseRunRunning || job.CurrentPhaseRunID != run.ID {
		return domain.CreateJobResponse{}, "", fmt.Errorf("only a running brainstorm can start the process: %w", ErrConflict)
	}
	if len(template.Phases) == 0 {
		return domain.CreateJobResponse{}, "", fmt.Errorf("the Template has no steps to start: %w", ErrConflict)
	}
	now := time.Now().UTC()
	run.Status = domain.PhaseRunAccepted
	run.Summary = goal
	run.CompletedAt = &now
	s.state.PhaseRuns[run.ID] = run
	session.Status = domain.SessionCompleted
	session.UpdatedAt = now
	s.state.Sessions[session.ID] = session
	job.Objective = goal
	next, nextRun := s.newWorkflowSessionLocked(&job, template, template.Phases[0], session.ID)
	s.state.Sessions[next.ID] = next
	s.state.PhaseRuns[nextRun.ID] = nextRun
	s.state.Jobs[job.ID] = job
	if err := s.saveLocked(); err != nil {
		return domain.CreateJobResponse{}, "", err
	}
	return domain.CreateJobResponse{Job: job, Session: next}, session.PreparedCompositionID, nil
}
