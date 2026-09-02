package server

import (
	"context"
	"crypto/subtle"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"slices"
	"strings"
	"time"

	"easyacp/internal/capsule"
	"easyacp/internal/domain"
	"easyacp/internal/store"
)

type workflowMCPRequest struct {
	JSONRPC string          `json:"jsonrpc"`
	ID      json.RawMessage `json:"id,omitempty"`
	Method  string          `json:"method"`
	Params  json.RawMessage `json:"params,omitempty"`
}

type workflowMCPResponse struct {
	JSONRPC string          `json:"jsonrpc"`
	ID      json.RawMessage `json:"id,omitempty"`
	Result  any             `json:"result,omitempty"`
	Error   *acpRPCError    `json:"error,omitempty"`
}

type workflowTool struct {
	Name        string         `json:"name"`
	Title       string         `json:"title,omitempty"`
	Description string         `json:"description"`
	InputSchema map[string]any `json:"inputSchema"`
}

func (s *Server) createWorkflowTemplate(w http.ResponseWriter, r *http.Request) {
	var req domain.CreateWorkflowTemplateRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	req.Operator = s.requestOperator(r, req.Operator)
	template, err := s.store.CreateWorkflowTemplate(req)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusCreated, template)
}

func (s *Server) updateWorkflowTemplate(w http.ResponseWriter, r *http.Request) {
	var req domain.CreateWorkflowTemplateRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	req.Operator = s.requestOperator(r, req.Operator)
	template, err := s.store.UpdateWorkflowTemplate(r.PathValue("templateID"), req)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, template)
}

func (s *Server) deleteWorkflowTemplate(w http.ResponseWriter, r *http.Request) {
	template, err := s.store.DeleteWorkflowTemplate(r.PathValue("templateID"), s.requestOperator(r, r.URL.Query().Get("operator")))
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, template)
}

func (s *Server) createDeliverableComment(w http.ResponseWriter, r *http.Request) {
	var req domain.CreateDeliverableCommentRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	author := s.requestOperator(r, req.Operator)
	comment, err := s.store.AddDeliverableComment(r.PathValue("deliverableID"), author, req)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusCreated, comment)
}

func (s *Server) answerWorkflowQuestion(w http.ResponseWriter, r *http.Request) {
	var req domain.AnswerWorkflowQuestionRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	questionID := r.PathValue("questionID")
	snapshot := s.store.Snapshot()
	questionIndex := slices.IndexFunc(snapshot.WorkflowQuestions, func(question domain.WorkflowQuestion) bool {
		return question.ID == questionID && question.Status == "open"
	})
	questions := snapshot.WorkflowQuestions
	if questionIndex < 0 || questionIndex >= len(questions) {
		writeError(w, store.ErrNotFound)
		return
	}
	if strings.EqualFold(req.Action, "accept") {
		if err := s.store.ValidateWorkflowQuestionTransition(questionID, req.Action); err != nil {
			writeError(w, err)
			return
		}
		if err := s.validateWorkflowAccept(questions[questionIndex].SessionID); err != nil {
			writeError(w, err)
			return
		}
		_, _, _, phase, _, _, err := s.store.WorkflowForSession(questions[questionIndex].SessionID)
		if err != nil {
			writeError(w, err)
			return
		}
		if phase.Executor != domain.WorkflowExecutorAction {
			operator := s.requestOperator(r, "")
			if _, err := s.acceptWorkflowWorkspace(r.Context(), questions[questionIndex].SessionID, "", "user:"+operator); err != nil {
				writeError(w, fmt.Errorf("integrate Session into Job branch before accept: %w", err))
				return
			}
		}
	}
	advance, err := s.store.AnswerWorkflowQuestion(questionID, s.requestOperator(r, ""), req.Action, req.Reason)
	if err != nil {
		writeError(w, err)
		return
	}
	if advance.NextSession != nil {
		go s.launchWorkflowSession(advance.NextSession.ID, advance.NextSession.Operator)
	}
	writeJSON(w, http.StatusOK, advance)
	if advance.NextSession == nil && advance.Question == nil && advance.Job.WorkflowStatus == domain.WorkflowDone {
		go s.retireWorkflowCompositions(advance.Job.ID, "")
	}
}

func (s *Server) workflowMCP(w http.ResponseWriter, r *http.Request) {
	sessionID := r.PathValue("sessionID")
	if !s.validWorkflowBearer(sessionID, r.Header.Get("Authorization")) {
		writeJSON(w, http.StatusUnauthorized, map[string]string{"error": "invalid workflow token"})
		return
	}
	if origin := strings.TrimSpace(r.Header.Get("Origin")); origin != "" {
		writeJSON(w, http.StatusForbidden, map[string]string{"error": "browser origins are not accepted by the internal MCP endpoint"})
		return
	}
	if r.Method != http.MethodPost {
		w.WriteHeader(http.StatusMethodNotAllowed)
		return
	}
	var request workflowMCPRequest
	decoder := json.NewDecoder(http.MaxBytesReader(w, r.Body, 3<<20))
	if err := decoder.Decode(&request); err != nil || request.JSONRPC != "2.0" || strings.TrimSpace(request.Method) == "" {
		writeJSON(w, http.StatusBadRequest, workflowMCPResponse{JSONRPC: "2.0", ID: request.ID, Error: &acpRPCError{Code: -32700, Message: "invalid JSON-RPC request"}})
		return
	}
	if len(request.ID) == 0 {
		w.WriteHeader(http.StatusAccepted)
		return
	}
	response := workflowMCPResponse{JSONRPC: "2.0", ID: request.ID}
	switch request.Method {
	case "initialize":
		var params struct {
			ProtocolVersion string `json:"protocolVersion"`
		}
		_ = json.Unmarshal(request.Params, &params)
		version := params.ProtocolVersion
		if version == "" {
			version = "2025-06-18"
		}
		response.Result = map[string]any{
			"protocolVersion": version,
			"capabilities":    map[string]any{"tools": map[string]bool{"listChanged": false}},
			"serverInfo":      map[string]string{"name": "spin-workflow", "title": "Spin Workflow", "version": "0.3.0"},
		}
	case "ping":
		response.Result = map[string]any{}
	case "tools/list":
		tools, err := s.workflowTools(sessionID)
		if err != nil {
			response.Error = mcpError(-32603, err)
		} else {
			response.Result = map[string]any{"tools": tools}
		}
	case "tools/call":
		var params struct {
			Name      string         `json:"name"`
			Arguments map[string]any `json:"arguments"`
		}
		if err := json.Unmarshal(request.Params, &params); err != nil {
			response.Error = mcpError(-32602, err)
		} else {
			result, err := s.callWorkflowTool(r.Context(), sessionID, params.Name, params.Arguments)
			if err != nil {
				response.Result = workflowToolResult(err.Error(), true)
			} else {
				response.Result = workflowToolResult(result, false)
			}
		}
	default:
		response.Error = &acpRPCError{Code: -32601, Message: "method not found"}
	}
	w.Header().Set("Content-Type", "application/json")
	writeJSON(w, http.StatusOK, response)
}

func mcpError(code int, err error) *acpRPCError {
	return &acpRPCError{Code: code, Message: err.Error()}
}

func workflowToolResult(text string, failed bool) map[string]any {
	return map[string]any{"content": []map[string]string{{"type": "text", "text": text}}, "isError": failed}
}

func (s *Server) workflowTools(sessionID string) ([]workflowTool, error) {
	_, _, run, phase, _, _, err := s.store.WorkflowForSession(sessionID)
	if err != nil {
		return nil, err
	}
	if run.Status != domain.PhaseRunRunning {
		return []workflowTool{}, nil
	}
	object := func(properties map[string]any, required ...string) map[string]any {
		schema := map[string]any{"type": "object", "properties": properties, "additionalProperties": false}
		if len(required) > 0 {
			schema["required"] = required
		}
		return schema
	}
	tools := []workflowTool{
		{Name: "ask", Title: "Vraag de gebruiker", Description: "Stel precies één noodzakelijke beslisvraag en pauzeer deze fase tot de gebruiker antwoordt.", InputSchema: object(map[string]any{"question": map[string]any{"type": "string", "description": "Eén concrete vraag"}}, "question")},
		{Name: "accept", Title: "Accepteer fase", Description: "Markeer deze fase als geslaagd en volg de geconfigureerde accept-overgang.", InputSchema: object(map[string]any{"summary": map[string]any{"type": "string"}})},
		{Name: "reject", Title: "Wijs fase af", Description: "Wijs deze fase af met een concrete reden en volg de geconfigureerde reject-overgang.", InputSchema: object(map[string]any{"reason": map[string]any{"type": "string"}}, "reason")},
	}
	if len(phase.Deliverables) > 0 {
		names := make([]string, 0, len(phase.Deliverables))
		for _, definition := range phase.Deliverables {
			names = append(names, definition.Name)
		}
		tools = append(tools, workflowTool{Name: "add_deliverable", Title: "Voeg deliverable toe", Description: "Bewaar een benoemd Markdown-document als nieuwe revisie bij deze Session.", InputSchema: object(map[string]any{
			"name":    map[string]any{"type": "string", "enum": names},
			"content": map[string]any{"type": "string", "description": "Volledige Markdown-inhoud"},
		}, "name", "content")})
	}
	return tools, nil
}

func (s *Server) callWorkflowTool(ctx context.Context, sessionID, name string, arguments map[string]any) (string, error) {
	name = strings.TrimSpace(name)
	stringArgument := func(key string) string {
		value, _ := arguments[key].(string)
		return strings.TrimSpace(value)
	}
	switch name {
	case "ask":
		question, err := s.store.AskWorkflowQuestion(sessionID, stringArgument("question"))
		if err != nil {
			return "", err
		}
		return "Vraag staat klaar voor de gebruiker: " + question.Question + ". Beëindig nu je beurt; dezelfde ACP Session wordt na het antwoord hervat.", nil
	case "add_deliverable":
		deliverable, err := s.store.AddWorkflowDeliverable(sessionID, stringArgument("name"), stringArgument("content"))
		if err != nil {
			return "", err
		}
		return fmt.Sprintf("Deliverable %s revisie %d is opgeslagen en zichtbaar als bijlage.", deliverable.Name, deliverable.Revision), nil
	case "accept", "reject":
		detail := stringArgument("summary")
		if name == "reject" {
			detail = stringArgument("reason")
		}
		if name == "accept" {
			if err := s.validateWorkflowAccept(sessionID); err != nil {
				return "", err
			}
			_, _, _, phase, _, _, err := s.store.WorkflowForSession(sessionID)
			if err != nil {
				return "", err
			}
			acceptTarget := strings.ToUpper(strings.TrimSpace(phase.Accept.Target))
			if !phase.Accept.AskUser && acceptTarget != domain.WorkflowTargetAskUser {
				if _, err := s.acceptWorkflowWorkspace(ctx, sessionID, detail, "agent"); err != nil {
					return "", fmt.Errorf("integrate Session into Job branch before accept: %w", err)
				}
			}
		}
		advance, err := s.store.CompleteWorkflowPhase(sessionID, name, detail)
		if err != nil {
			return "", err
		}
		if advance.NextSession != nil {
			go s.launchWorkflowSession(advance.NextSession.ID, advance.NextSession.Operator)
			return fmt.Sprintf("Fase afgerond. %s is als nieuwe Session gestart.", advance.PhaseRun.PhaseName), nil
		}
		if advance.Question != nil {
			return "De workflow wacht op de gebruiker: " + advance.Question.Question, nil
		}
		if advance.Job.WorkflowStatus == domain.WorkflowDone {
			time.AfterFunc(1500*time.Millisecond, func() { s.retireWorkflowCompositions(advance.Job.ID, "") })
		}
		return "Workflow afgerond.", nil
	default:
		return "", fmt.Errorf("unknown workflow tool %q", name)
	}
}

func (s *Server) validateWorkflowAccept(sessionID string) error {
	_, _, run, phase, deliverables, _, err := s.store.WorkflowForSession(sessionID)
	if err != nil {
		return err
	}
	for _, required := range phase.Deliverables {
		if !required.Required {
			continue
		}
		present := slices.ContainsFunc(deliverables, func(deliverable domain.Deliverable) bool {
			return deliverable.PhaseRunID == run.ID && strings.EqualFold(deliverable.Name, required.Name)
		})
		if !present {
			return fmt.Errorf("required deliverable %s is missing", required.Name)
		}
	}
	return s.store.ValidateWorkflowPhaseTransition(sessionID, "accept")
}

func phaseAllowsChanges(phase domain.WorkflowPhase) bool {
	return phase.AllowChanges || phase.AllowCommit
}

func (s *Server) acceptWorkflowWorkspace(ctx context.Context, sessionID, summary, acceptedBy string) (capsule.WorkspaceAcceptanceResult, error) {
	job, _, run, phase, _, _, err := s.store.WorkflowForSession(sessionID)
	if err != nil {
		return capsule.WorkspaceAcceptanceResult{}, err
	}
	if run.Status != domain.PhaseRunRunning && run.Status != domain.PhaseRunPending {
		return capsule.WorkspaceAcceptanceResult{}, fmt.Errorf("phase is %s: %w", run.Status, store.ErrConflict)
	}
	_, composition, err := s.sessionComposition(sessionID, job.Owner)
	if err != nil {
		return capsule.WorkspaceAcceptanceResult{}, err
	}
	acceptor, ok := s.engine.(capsule.WorkspaceAcceptor)
	if !ok || composition.Runtime == nil {
		return capsule.WorkspaceAcceptanceResult{}, fmt.Errorf("capsule engine cannot accept workspaces: %w", store.ErrConflict)
	}
	authentication := &capsule.GitAuthentication{}
	if account, authenticated, accountErr := s.gitAccountForWorkspace(ctx, composition.Git, composition.Operator); accountErr != nil {
		return capsule.WorkspaceAcceptanceResult{}, accountErr
	} else if authenticated {
		username := account.Login
		if account.Provider == "gitlab" {
			username = "oauth2"
		}
		authentication = &capsule.GitAuthentication{
			Username: username, Password: account.AccessToken,
			AuthorName: account.Name, AuthorEmail: account.Email,
		}
	} else if composition.Git != nil {
		authentication.AuthorName = composition.Git.AuthorName
		authentication.AuthorEmail = composition.Git.AuthorEmail
	}
	summary = strings.TrimSpace(summary)
	if summary == "" {
		summary = strings.TrimSpace(run.Summary)
	}
	if summary == "" {
		summary = "Phase accepted"
	}
	if len(summary) > 2000 {
		summary = summary[:2000]
	}
	acceptedBy = strings.TrimSpace(acceptedBy)
	if acceptedBy == "" {
		acceptedBy = "agent"
	}
	commitBody := fmt.Sprintf("%s\n\nSpin-Job: %s\nSpin-Session: %s\nSpin-Phase: %s\nSpin-Accepted-By: %s", summary, job.ID, sessionID, phase.ID, acceptedBy)
	acceptContext, cancel := context.WithTimeout(ctx, 60*time.Second)
	defer cancel()
	return acceptor.AcceptWorkspace(acceptContext, *composition.Runtime, capsule.WorkspaceAcceptance{
		AllowChanges: phaseAllowsChanges(phase), CommitSubject: "workflow(" + phase.ID + "): accepted",
		CommitBody: commitBody, RemoteRef: job.Branch, Authentication: authentication,
	})
}

func (s *Server) launchWorkflowSession(sessionID, operator string) {
	s.launchWorkflowSessionContext(context.Background(), sessionID, operator)
}

func (s *Server) launchWorkflowSessionContext(ctx context.Context, sessionID, operator string) {
	operator = normalizeOperator(operator)
	snapshot := s.store.Snapshot()
	sessionIndex := slices.IndexFunc(snapshot.Sessions, func(session domain.Session) bool { return session.ID == sessionID })
	if sessionIndex < 0 {
		return
	}
	session := snapshot.Sessions[sessionIndex]
	if operator == "" {
		operator = session.Operator
	}
	if _, _, _, phase, _, _, err := s.store.WorkflowForSession(session.ID); err == nil && phase.Executor == domain.WorkflowExecutorAction {
		s.retireWorkflowCompositions(session.JobID, session.ID)
		s.launchWorkflowAction(ctx, session.ID)
		return
	}
	if _, ok := s.engine.(capsule.EnabledEngine); !ok {
		return
	}
	needsMaterialization := session.PreparedCompositionID == ""
	if !needsMaterialization {
		compositionIndex := slices.IndexFunc(snapshot.Compositions, func(composition domain.Composition) bool {
			return composition.ID == session.PreparedCompositionID
		})
		needsMaterialization = compositionIndex < 0 || snapshot.Compositions[compositionIndex].Runtime == nil || snapshot.Compositions[compositionIndex].Runtime.Status == "stopped"
	}
	if needsMaterialization {
		materializeContext, cancel := context.WithTimeout(ctx, 2*time.Minute)
		_, materializeErr := s.useCapsule(materializeContext, domain.UseRequest{Selector: "session:" + session.ID, Operator: operator})
		cancel()
		if materializeErr != nil {
			if !errors.Is(materializeErr, context.Canceled) {
				s.logger.Warn("materialize workflow phase", "session", session.ID, "error", materializeErr)
			}
			return
		}
	}
	if ctx.Err() != nil {
		return
	}
	if _, err := s.store.MarkWorkflowPhaseRunning(session.ID); err != nil {
		s.logger.Warn("mark workflow phase running", "session", session.ID, "error", err)
		return
	}
	active, err := s.getOrStartACP(session.ID, operator)
	if err != nil {
		s.logger.Warn("start workflow ACP", "session", session.ID, "error", err)
		return
	}
	prompt, err := s.workflowPromptForACP(session.ID, active.promptCapabilities())
	if err != nil {
		s.logger.Warn("build workflow prompt", "session", session.ID, "error", err)
		return
	}
	if err := s.startACPPrompt(active, prompt); err != nil {
		s.logger.Warn("start workflow prompt", "session", session.ID, "error", err)
		return
	}
	s.retireWorkflowCompositions(session.JobID, session.ID)
}

func (s *Server) retireWorkflowCompositions(jobID, keepSessionID string) {
	snapshot := s.store.Snapshot()
	jobIndex := slices.IndexFunc(snapshot.Jobs, func(job domain.Job) bool { return job.ID == jobID })
	if jobIndex < 0 {
		return
	}
	sessionIDs := map[string]bool{}
	for _, sessionID := range snapshot.Jobs[jobIndex].SessionIDs {
		if sessionID != keepSessionID {
			sessionIDs[sessionID] = true
		}
	}
	for _, composition := range snapshot.Compositions {
		if !sessionIDs[composition.SessionID] || composition.Runtime == nil || composition.Runtime.Status == "stopped" {
			continue
		}
		ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
		if _, err := s.stopCapsule(ctx, composition.ID, composition.Operator); err != nil {
			s.logger.Warn("retire workflow composition", "job", jobID, "session", composition.SessionID, "error", err)
		}
		cancel()
	}
}

func (s *Server) workflowPrompt(sessionID string) (string, error) {
	return s.workflowPromptWithOptions(sessionID, false)
}

func (s *Server) workflowPromptForACP(sessionID string, capabilities acpPromptCapabilities) (string, error) {
	return s.workflowPromptWithOptions(sessionID, capabilities.EmbeddedContext)
}

func (s *Server) workflowPromptWithOptions(sessionID string, attachInjectedDeliverables bool) (string, error) {
	job, _, run, phase, deliverables, questions, err := s.store.WorkflowForSession(sessionID)
	if err != nil {
		return "", err
	}
	latest := map[string]domain.Deliverable{}
	for _, deliverable := range deliverables {
		key := strings.ToLower(strings.TrimSpace(deliverable.Name))
		if current, ok := latest[key]; !ok || deliverable.Revision > current.Revision {
			latest[key] = deliverable
		}
	}
	var prompt strings.Builder
	fmt.Fprintf(&prompt, "Je voert Spin workflowfase %q uit (poging %d).\n\nJOB\nNaam: %s\nGoal: %s\n\nINSTRUCTIES\n%s\n", phase.Name, run.Attempt, job.Title, job.Objective, phase.Instructions)
	snapshot := s.store.Snapshot()
	if job.ForkedFromJobID != "" {
		sourceIndex := slices.IndexFunc(snapshot.Jobs, func(candidate domain.Job) bool { return candidate.ID == job.ForkedFromJobID })
		if sourceIndex < 0 {
			return "", fmt.Errorf("fork source Job %s is unavailable", job.ForkedFromJobID)
		}
		source := snapshot.Jobs[sourceIndex]
		fmt.Fprintf(&prompt, "\nVERVOLGCONTEXT\nDeze Job is een vervolg op de afgesloten Job %q. Werk vanaf diens remote resultaatbranch %s.\nOorspronkelijke goal: %s\n", source.Title, source.Branch, source.Objective)
		if sourceAttachments := s.store.JobAttachments(source.ID); len(sourceAttachments) > 0 {
			prompt.WriteString("Bijlagen uit die Job zijn opnieuw read-only beschikbaar:\n")
			for _, attachment := range sourceAttachments {
				fmt.Fprintf(&prompt, "- %s (%s): %s\n", attachment.Name, attachment.MediaType, attachment.CapsulePath)
			}
		}
		sourceLatest := map[string]domain.Deliverable{}
		for _, deliverable := range snapshot.Deliverables {
			if deliverable.JobID != source.ID {
				continue
			}
			key := strings.ToLower(strings.TrimSpace(deliverable.Name))
			if current, ok := sourceLatest[key]; !ok || deliverable.Revision > current.Revision {
				sourceLatest[key] = deliverable
			}
		}
		if len(sourceLatest) > 0 {
			prompt.WriteString("Gebruik ook de laatste documenten uit die Job als context:\n")
			sourceDeliverables := make([]domain.Deliverable, 0, len(sourceLatest))
			for _, deliverable := range sourceLatest {
				sourceDeliverables = append(sourceDeliverables, deliverable)
			}
			slices.SortFunc(sourceDeliverables, func(a, b domain.Deliverable) int {
				return strings.Compare(strings.ToLower(a.Name), strings.ToLower(b.Name))
			})
			for _, deliverable := range sourceDeliverables {
				if attachInjectedDeliverables {
					fmt.Fprintf(&prompt, "- %s (revisie %d) is als Markdown-bijlage aan dit ACP-bericht toegevoegd.\n", deliverable.Name, deliverable.Revision)
				} else {
					fmt.Fprintf(&prompt, "\n--- Vorige Job · %s (revisie %d) ---\n%s\n", deliverable.Name, deliverable.Revision, deliverable.Content)
				}
			}
		}
		for _, sourceRun := range snapshot.PhaseRuns {
			if sourceRun.JobID == source.ID && sourceRun.ActionResult != nil && sourceRun.ActionResult.URL != "" {
				fmt.Fprintf(&prompt, "Remote resultaat: %s\n", sourceRun.ActionResult.URL)
			}
		}
	}
	attachments := s.store.JobAttachments(job.ID)
	if len(attachments) > 0 {
		prompt.WriteString("\nJOB-BIJLAGEN\nDeze immutable bestanden zijn door een gebruiker als bron toegevoegd. Bekijk de relevante bijlagen daadwerkelijk; wijzig ze niet en kopieer ze niet naar Git.\n")
		for _, attachment := range attachments {
			fmt.Fprintf(&prompt, "- %s (%s, %d bytes): %s\n", attachment.Name, attachment.MediaType, attachment.Size, attachment.CapsulePath)
		}
	}
	if phaseAllowsChanges(phase) {
		prompt.WriteString("\nREPOSITORYBELEID\nJe mag bestanden in de repository wijzigen. Commit of push niet zelf: bij accept maakt Spin zo nodig één resultaatcommit en publiceert die naar de Job-branch.\n")
	} else {
		prompt.WriteString("\nREPOSITORYBELEID\nJe mag de repository niet wijzigen. Werk uitsluitend met inspectie en vragen")
		if len(phase.Deliverables) > 0 {
			prompt.WriteString(", en lever de hieronder gevraagde documenten op")
		}
		prompt.WriteString("; accept wordt geblokkeerd wanneer deze Session van zijn Git-basis afwijkt.\n")
	}
	if len(phase.Deliverables) > 0 {
		prompt.WriteString("\nOP TE LEVEREN\nMaak de onderstaande documenten volledig in Markdown en sla ieder document op met add_deliverable. Gebruik de naam exact zoals vermeld.\n")
		for _, definition := range phase.Deliverables {
			requirement := "OPTIONEEL"
			if definition.Required {
				requirement = "VERPLICHT"
			}
			description := strings.TrimSpace(definition.Description)
			if description == "" {
				description = "Markdown-document voor deze workflowfase"
			}
			fmt.Fprintf(&prompt, "- %s (%s): %s\n  Tool: add_deliverable met name %q en de volledige Markdown-inhoud.\n", definition.Name, requirement, description, definition.Name)
		}
	}
	if len(phase.Inject) > 0 {
		prompt.WriteString("\nGEÏNJECTEERDE DELIVERABLES\nDeze documenten zijn verplichte context voor deze fase; gebruik steeds de laatste revisie.\n")
		for _, name := range phase.Inject {
			deliverable, ok := latest[strings.ToLower(strings.TrimSpace(name))]
			if !ok {
				return "", fmt.Errorf("injected deliverable %s is not available for phase %s", name, phase.Name)
			}
			if attachInjectedDeliverables {
				fmt.Fprintf(&prompt, "- %s (revisie %d) is als verplichte Markdown-bijlage aan dit ACP-bericht toegevoegd.\n", deliverable.Name, deliverable.Revision)
			} else {
				fmt.Fprintf(&prompt, "\n--- %s (revisie %d) ---\n%s\n", deliverable.Name, deliverable.Revision, deliverable.Content)
			}
		}
	}
	commentsWritten := false
	for _, deliverable := range deliverables {
		current := latest[strings.ToLower(strings.TrimSpace(deliverable.Name))]
		if current.ID != deliverable.ID {
			continue
		}
		comments := make([]domain.DeliverableComment, 0)
		for _, comment := range snapshot.DeliverableComments {
			if comment.DeliverableID == deliverable.ID {
				comments = append(comments, comment)
			}
		}
		if len(comments) == 0 {
			continue
		}
		if !commentsWritten {
			prompt.WriteString("\nCOMMENTS OP ACTUELE DELIVERABLES\nVerwerk deze opmerkingen. Ze horen bij de momenteel laatste revisies en staan los van de ACCEPT/REJECT-route.\n")
			commentsWritten = true
		}
		fmt.Fprintf(&prompt, "\n%s (revisie %d)\n", deliverable.Name, deliverable.Revision)
		for _, comment := range comments {
			quote := strings.ReplaceAll(strings.TrimSpace(comment.SelectedText), "\n", "\n  > ")
			body := strings.ReplaceAll(comment.Body, "\n", "\n  ")
			fmt.Fprintf(&prompt, "- %s bij:\n  > %s\n  %s\n", comment.Author, quote, body)
		}
	}
	answered := make([]domain.WorkflowQuestion, 0)
	for _, question := range questions {
		if question.Status == "answered" {
			answered = append(answered, question)
		}
	}
	if len(answered) > 0 {
		prompt.WriteString("\nVASTGELEGDE BESLUITEN\n")
		for _, question := range answered {
			decision := strings.ToUpper(question.Answer)
			if question.Reason != "" {
				decision += ": " + question.Reason
			}
			fmt.Fprintf(&prompt, "- %s → %s\n", question.Question, decision)
		}
	}
	history := snapshot.PhaseRuns
	feedbackWritten := false
	for _, previous := range history {
		if previous.JobID != job.ID || previous.ID == run.ID || previous.RejectReason == "" {
			continue
		}
		if !feedbackWritten {
			prompt.WriteString("\nFEEDBACK UIT EERDERE POGINGEN\n")
			feedbackWritten = true
		}
		fmt.Fprintf(&prompt, "- %s, poging %d: %s\n", previous.PhaseName, previous.Attempt, previous.RejectReason)
	}
	prompt.WriteString("\nWERKWIJZE\nGebruik uitsluitend de aangeboden Spin workflowtools om workflowstate te wijzigen. ask stelt precies één vraag; bundel geen vragen. ")
	if len(phase.Deliverables) > 0 {
		prompt.WriteString("Lever ieder hierboven gevraagd document volledig als Markdown aan met add_deliverable. ")
	} else {
		prompt.WriteString("Deze fase vraagt geen deliverables; add_deliverable is daarom niet beschikbaar en je hoeft geen document op te leveren. ")
	}
	prompt.WriteString("Commit of push nooit zelf. Sluit de fase altijd af met accept, of reject met een concrete reden. ACCEPT laat Spin de Session gecontroleerd in de Job-branch opnemen.\n")
	return prompt.String(), nil
}

func (s *Server) workflowMCPServer(sessionID string) (domain.MCPServer, error) {
	if s.internalURL == "" {
		return domain.MCPServer{}, errors.New("SPIN_INTERNAL_URL is not configured")
	}
	token, err := randomOAuthValue(32)
	if err != nil {
		return domain.MCPServer{}, err
	}
	s.workflowMu.Lock()
	s.workflowTokens[sessionID] = secretHash(token)
	s.workflowMu.Unlock()
	return domain.MCPServer{
		Name: "spin-workflow", Transport: domain.MCPTransportHTTP,
		URL:     s.internalURL + "/api/workflow/mcp/" + sessionID,
		Headers: []domain.MCPSecret{{Name: "Authorization", Value: "Bearer " + token}},
	}, nil
}

func (s *Server) validWorkflowBearer(sessionID, header string) bool {
	if !strings.HasPrefix(header, "Bearer ") {
		return false
	}
	s.workflowMu.Lock()
	expected := s.workflowTokens[sessionID]
	s.workflowMu.Unlock()
	actual := secretHash(strings.TrimSpace(strings.TrimPrefix(header, "Bearer ")))
	return expected != "" && subtle.ConstantTimeCompare([]byte(expected), []byte(actual)) == 1
}
