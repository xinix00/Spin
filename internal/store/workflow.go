package store

import (
	"fmt"
	"slices"
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
			if input.Action != nil && strings.EqualFold(strings.TrimSpace(input.Action.Type), domain.WorkflowActionGitPullRequest) {
				// The PR finalizer is control-plane policy, not editable Template
				// input. Accept echoed Templates by dropping the generated phase.
				continue
			}
			return "", "", "", nil, fmt.Errorf("system actions cannot be configured as Template phases: %w", ErrConflict)
		}
		inputs = append(inputs, input)
	}
	if operator == "" || name == "" || len(inputs) == 0 {
		return "", "", "", nil, fmt.Errorf("operator, name and at least one phase are required: %w", ErrConflict)
	}
	phases := make([]domain.WorkflowPhase, len(inputs))
	phaseIDs := map[string]bool{domain.WorkflowPullRequestPhaseID: true}
	availableDeliverables := map[string]string{}
	for index, input := range inputs {
		phase := input
		if phase.Executor == "" {
			phase.Executor = domain.WorkflowExecutorAgent
		}
		phase.AllowChanges = phase.AllowChanges || phase.AllowCommit
		phase.AllowCommit = false
		if phase.AskUser {
			phase.Accept.AskUser = true
			phase.Reject.AskUser = true
			phase.AskUser = false
		}
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
		case domain.WorkflowExecutorAction:
			return "", "", "", nil, fmt.Errorf("system actions cannot be configured as Template phases: %w", ErrConflict)
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
		phase.Accept.Target = workflowPullRequestTarget(phase.Accept.Target)
		phase.Accept.Exhausted = workflowPullRequestTarget(phase.Accept.Exhausted)
		phase.Reject.Target = workflowPullRequestTarget(phase.Reject.Target)
		phase.Reject.Exhausted = workflowPullRequestTarget(phase.Reject.Exhausted)
		for _, transition := range []domain.WorkflowTransition{phase.Accept, phase.Reject} {
			for _, target := range []string{transition.Target, transition.Exhausted} {
				if target != "" && !validWorkflowTarget(target, phaseIDs) {
					return "", "", "", nil, fmt.Errorf("phase %s points to unknown target %q: %w", phase.Name, target, ErrConflict)
				}
			}
		}
	}
	phases = append(phases, workflowPullRequestPhase())
	return operator, name, strings.TrimSpace(req.Description), phases, nil
}

func workflowPullRequestTarget(target string) string {
	if strings.EqualFold(strings.TrimSpace(target), domain.WorkflowTargetDone) {
		return domain.WorkflowPullRequestPhaseID
	}
	return target
}

func workflowPullRequestPhase() domain.WorkflowPhase {
	return domain.WorkflowPhase{
		ID:       domain.WorkflowPullRequestPhaseID,
		Name:     "Pull request",
		Executor: domain.WorkflowExecutorAction,
		Action:   &domain.WorkflowAction{Type: domain.WorkflowActionGitPullRequest},
		Accept:   domain.WorkflowTransition{Target: domain.WorkflowTargetDone},
		Reject: domain.WorkflowTransition{
			Target: domain.WorkflowTargetSelf, Max: 2, Exhausted: domain.WorkflowTargetAskUser,
		},
	}
}

func ensureWorkflowPullRequestFinalizer(template domain.WorkflowTemplate) (domain.WorkflowTemplate, bool) {
	finalizerIndex := -1
	for index, phase := range template.Phases {
		if phase.Executor == domain.WorkflowExecutorAction && phase.Action != nil && phase.Action.Type == domain.WorkflowActionGitPullRequest {
			finalizerIndex = index
			break
		}
	}
	changed := false
	var finalizer domain.WorkflowPhase
	if finalizerIndex < 0 {
		finalizer = workflowPullRequestPhase()
		changed = true
	} else {
		finalizer = template.Phases[finalizerIndex]
		if finalizer.ID == "" {
			finalizer.ID = domain.WorkflowPullRequestPhaseID
			changed = true
		}
		if finalizerIndex != len(template.Phases)-1 {
			template.Phases = append(template.Phases[:finalizerIndex], template.Phases[finalizerIndex+1:]...)
			changed = true
		} else {
			template.Phases = template.Phases[:finalizerIndex]
		}
	}
	for index := range template.Phases {
		phase := &template.Phases[index]
		for _, transition := range []*domain.WorkflowTransition{&phase.Accept, &phase.Reject} {
			if strings.EqualFold(strings.TrimSpace(transition.Target), domain.WorkflowTargetDone) {
				transition.Target = finalizer.ID
				changed = true
			}
			if strings.EqualFold(strings.TrimSpace(transition.Exhausted), domain.WorkflowTargetDone) {
				transition.Exhausted = finalizer.ID
				changed = true
			}
		}
	}
	template.Phases = append(template.Phases, finalizer)
	return template, changed
}

func (s *Store) backfillWorkflowPullRequestsLocked() {
	for jobID, existingJob := range s.state.Jobs {
		if existingJob.TemplateID == "" || existingJob.Status == domain.JobCancelled || (existingJob.Status != domain.JobDone && existingJob.WorkflowStatus != domain.WorkflowDone) {
			continue
		}
		template, ok := s.workflowTemplateForJobLocked(existingJob)
		if !ok {
			continue
		}
		finalizer, ok := workflowPhase(template, domain.WorkflowPullRequestPhaseID)
		if !ok || finalizer.Executor != domain.WorkflowExecutorAction {
			continue
		}
		alreadyFinalized := false
		for _, run := range s.state.PhaseRuns {
			if run.JobID == existingJob.ID && run.PhaseID == finalizer.ID {
				alreadyFinalized = true
				break
			}
		}
		if alreadyFinalized {
			continue
		}
		parentSessionID := ""
		for index := len(existingJob.PhaseRunIDs) - 1; index >= 0; index-- {
			if run, exists := s.state.PhaseRuns[existingJob.PhaseRunIDs[index]]; exists {
				parentSessionID = run.SessionID
				break
			}
		}
		job := existingJob
		session, run := s.newWorkflowSessionLocked(&job, template, finalizer, parentSessionID)
		s.state.Jobs[jobID] = job
		s.state.Sessions[session.ID] = session
		s.state.PhaseRuns[run.ID] = run
	}
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
	for _, phase := range template.Phases {
		if phase.ID == phaseID {
			return phase, true
		}
	}
	return domain.WorkflowPhase{}, false
}

func workflowPhaseEnvironment(phase domain.WorkflowPhase, fallbackSelector string, fallbackWith []string) (string, []string) {
	selector := phase.EnvironmentSelector
	if selector == "" {
		selector = fallbackSelector
	}
	with := append([]string(nil), fallbackWith...)
	with = append(with, phase.WithSelectors...)
	return selector, uniqueStrings(with)
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
func (s *Store) RetryWorkflowSession(sessionID, operator string) (domain.CreateJobResponse, string, error) {
	operator = normalizeSubject(operator)
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
	if operator == "" || job.Owner != operator {
		return domain.CreateJobResponse{}, "", ErrConflict
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

func (s *Store) AddWorkflowDeliverable(sessionID, name, content string) (domain.Deliverable, error) {
	name = strings.TrimSpace(name)
	content = strings.TrimSpace(content)
	if name == "" || content == "" || len(content) > maxDeliverableBytes {
		return domain.Deliverable{}, fmt.Errorf("deliverable name and content are required and content may be at most %d bytes: %w", maxDeliverableBytes, ErrConflict)
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	session, ok := s.state.Sessions[sessionID]
	if !ok {
		return domain.Deliverable{}, ErrNotFound
	}
	job, _, run, phase, err := s.workflowLocked(session)
	if err != nil {
		return domain.Deliverable{}, err
	}
	if run.Status != domain.PhaseRunRunning {
		return domain.Deliverable{}, fmt.Errorf("phase is %s: %w", run.Status, ErrConflict)
	}
	definition := domain.DeliverableDefinition{}
	for _, candidate := range phase.Deliverables {
		if strings.EqualFold(candidate.Name, name) {
			definition = candidate
			break
		}
	}
	if definition.Name == "" {
		return domain.Deliverable{}, fmt.Errorf("deliverable %q is not declared by phase %s: %w", name, phase.Name, ErrConflict)
	}
	revision := 1
	for _, existing := range s.state.Deliverables {
		if existing.JobID == job.ID && strings.EqualFold(existing.Name, definition.Name) && existing.Revision >= revision {
			revision = existing.Revision + 1
		}
	}
	deliverable := domain.Deliverable{
		ID: newID("del"), JobID: job.ID, PhaseRunID: run.ID, SessionID: session.ID,
		Name: definition.Name, Description: definition.Description, Content: content, Revision: revision, CreatedAt: time.Now().UTC(),
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
	if result.Type != phase.Action.Type || result.URL == "" {
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
	if author == "" || body == "" || strings.TrimSpace(req.SelectedText) == "" {
		return domain.DeliverableComment{}, fmt.Errorf("author, selected text and comment are required: %w", ErrConflict)
	}
	if req.StartOffset < 0 || req.EndOffset <= req.StartOffset {
		return domain.DeliverableComment{}, fmt.Errorf("comment selection offsets are invalid: %w", ErrConflict)
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
	needsUser := transition.AskUser || resolveWorkflowTarget(template, phase.ID, transition.Target) == domain.WorkflowTargetAskUser
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
		// A failed mandatory finalizer cannot be approved away: either the
		// user retries it or the Job remains pending without a false DONE.
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
	environmentSelector, withSelectors := workflowPhaseEnvironment(phase, job.EnvironmentSelector, job.WithSelectors)
	_, tool, _ := parseArtifactSelector(environmentSelector)
	if phase.Executor == domain.WorkflowExecutorAction {
		environmentSelector, withSelectors, tool = "", nil, ""
	}
	namespace := strings.TrimSuffix(job.Branch, "/main")
	session := domain.Session{
		ID: sessionID, JobID: job.ID, PhaseRunID: runID, ParentSessionID: parentSessionID,
		SpawnedBySessionID: parentSessionID, ForkMode: domain.ForkRoot, Tool: tool,
		Executor: phase.Executor, EnvironmentSelector: environmentSelector, WithSelectors: withSelectors,
		MCPServerIDs: append([]string{}, job.MCPServerIDs...), Role: phase.Name, Model: job.Model,
		Operator: job.Owner, ObjectiveDelta: phase.Instructions, GitRepositoryID: job.GitRepositoryID,
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
