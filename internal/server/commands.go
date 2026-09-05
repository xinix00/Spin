package server

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"strconv"
	"strings"

	"easyacp/internal/domain"
	"easyacp/internal/store"
)

func (s *Server) executeCommand(w http.ResponseWriter, r *http.Request) {
	var req domain.CommandRequest
	if !decodeJSON(w, r, &req) {
		return
	}
	req.Operator = s.requestOperator(r, req.Operator)
	response, err := s.runCommandContext(r.Context(), req)
	if err != nil {
		writeError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, response)
}

func (s *Server) runCommand(req domain.CommandRequest) (domain.CommandResponse, error) {
	return s.runCommandContext(context.Background(), req)
}

func (s *Server) runCommandContext(ctx context.Context, req domain.CommandRequest) (domain.CommandResponse, error) {
	line := strings.TrimSpace(req.Line)
	actor := strings.TrimSpace(req.Operator)
	if line == "" || actor == "" {
		return domain.CommandResponse{}, fmt.Errorf("operator and command line are required: %w", store.ErrConflict)
	}
	fields := strings.Fields(line)
	verb := strings.ToUpper(fields[0])

	switch verb {
	case "RECORD":
		if len(fields) < 2 {
			return domain.CommandResponse{}, fmt.Errorf("usage: RECORD kind:name [--scope=user|global] [--from=kind:name] [--enable=git|acp]: %w", store.ErrConflict)
		}
		kind, name, flagStart, err := recordTarget(fields)
		if err != nil {
			return domain.CommandResponse{}, err
		}
		flags := commandFlags(fields[flagStart:])
		scope := domain.ArtifactScope(flags["scope"])
		if flags["user"] == "true" {
			scope = domain.ScopeUser
		}
		if flags["global"] == "true" {
			scope = domain.ScopeGlobal
		}
		parentIDs := []string{}
		if selector := strings.TrimSpace(flags["from"]); selector != "" {
			parentKind, parentName, ok := strings.Cut(selector, ":")
			if !ok || parentKind == "" || parentName == "" {
				return domain.CommandResponse{}, fmt.Errorf("--from must be kind:name, for example --from=tool:node: %w", store.ErrConflict)
			}
			parentProfile := flags["profile"]
			if parentProfile == "" {
				parentProfile = "default"
			}
			parent, err := s.store.LatestArtifact(domain.ArtifactKind(strings.ToLower(parentKind)), parentName, actor, parentProfile)
			if err != nil {
				return domain.CommandResponse{}, fmt.Errorf("resolve parent %s: %w", selector, err)
			}
			parentIDs = append(parentIDs, parent.ID)
		}
		enables, err := commandEnablements(flags)
		if err != nil {
			return domain.CommandResponse{}, err
		}
		recording, err := s.createCapsuleRecording(ctx, domain.CreateRecordingRequest{
			Actor: actor, Kind: kind, Name: name,
			Scope: scope, Subject: flags["subject"], Profile: flags["profile"],
			ParentArtifactIDs: parentIDs, CompatibilityFingerprint: flags["compatibility"], Enables: enables,
		})
		if err != nil {
			return domain.CommandResponse{}, err
		}
		message := fmt.Sprintf("● RECORDING %s:%s · scope=%s · base=%s · type commands, then END RECORD", recording.Kind, recording.Name, recording.Scope, recording.Runtime.BaseRef)
		if len(recording.Enables) > 0 {
			message += " · ENABLES=" + enablementNames(recording.Enables)
		}
		if recording.Runtime != nil && recording.Runtime.ContainerID != "" {
			message += "\nlive Capsule PTY ready"
		}
		return domain.CommandResponse{
			Message:   message,
			Recording: &recording,
		}, nil

	case "EDIT":
		// EDIT records the current version of a layer again, with every setting
		// it already has, and END RECORD makes the result the new version.
		if len(fields) < 2 {
			return domain.CommandResponse{}, fmt.Errorf("usage: EDIT kind:name [--profile=default]: %w", store.ErrConflict)
		}
		kind, name, flagStart, err := recordTarget(append([]string{"RECORD"}, fields[1:]...))
		if err != nil {
			return domain.CommandResponse{}, err
		}
		flags := commandFlags(fields[flagStart:])
		profile := flags["profile"]
		if profile == "" {
			profile = "default"
		}
		current, err := s.store.LatestArtifact(kind, name, actor, profile)
		if err != nil {
			return domain.CommandResponse{}, fmt.Errorf("EDIT %s:%s: %w", kind, name, err)
		}
		recording, err := s.createCapsuleRecording(ctx, domain.CreateRecordingRequest{
			Actor: actor, Kind: current.Kind, Name: current.Name,
			Scope: current.Scope, Subject: current.Subject, Profile: current.Profile,
			Provides: current.Provides, Requires: current.Requires, Enables: current.Enables, Slot: current.Slot,
			ParentArtifactIDs: []string{current.ID}, CompatibilityFingerprint: current.CompatibilityFingerprint,
			Sensitivity: current.Sensitivity, ReplacesArtifactID: current.ID,
		})
		if err != nil {
			return domain.CommandResponse{}, err
		}
		message := fmt.Sprintf("● EDITING %s:%s · scope=%s · from current version %s · type what you want to add, then END RECORD replaces the current version", recording.Kind, recording.Name, recording.Scope, current.SnapshotDigest)
		if len(recording.Enables) > 0 {
			message += " · ENABLES=" + enablementNames(recording.Enables)
		}
		return domain.CommandResponse{Message: message, Recording: &recording}, nil

	case "END":
		if len(fields) != 2 || strings.ToUpper(fields[1]) != "RECORD" {
			return domain.CommandResponse{}, fmt.Errorf("usage: END RECORD: %w", store.ErrConflict)
		}
		recording, err := s.store.OpenRecording(actor)
		if err != nil {
			return domain.CommandResponse{}, err
		}
		artifact, err := s.endCapsuleRecording(ctx, recording.ID, domain.EndRecordingRequest{Actor: actor})
		if err != nil {
			return domain.CommandResponse{}, err
		}
		return domain.CommandResponse{
			Message:  fmt.Sprintf("saved %s:%s/%s as %s · driver=%s · process-state=%t", artifact.Kind, artifact.Name, artifact.Profile, artifact.SnapshotDigest, artifact.Snapshot.Driver, artifact.Snapshot.IncludesProcessState),
			Artifact: &artifact,
		}, nil

	case "CANCEL":
		if len(fields) != 2 || strings.ToUpper(fields[1]) != "RECORD" {
			return domain.CommandResponse{}, fmt.Errorf("usage: CANCEL RECORD: %w", store.ErrConflict)
		}
		recording, err := s.store.OpenRecording(actor)
		if err != nil {
			return domain.CommandResponse{}, err
		}
		cancelled, err := s.cancelCapsuleRecording(ctx, recording.ID, domain.CancelRecordingRequest{Actor: actor})
		if err != nil {
			return domain.CommandResponse{}, err
		}
		return domain.CommandResponse{Message: "recording cancelled", Recording: &cancelled}, nil

	case "FROM":
		if len(fields) < 2 || len(fields) > 3 {
			return domain.CommandResponse{}, fmt.Errorf("usage: FROM kind:name: %w", store.ErrConflict)
		}
		kind, name, _, err := recordTarget(append([]string{"RECORD"}, fields[1:]...))
		if err != nil {
			return domain.CommandResponse{}, err
		}
		recording, err := s.store.OpenRecording(actor)
		if err != nil {
			return domain.CommandResponse{}, err
		}
		updated, err := s.attachCapsuleParent(ctx, recording.ID, domain.AttachRecordingParentRequest{
			Actor: actor, Kind: kind, Name: name,
		})
		if err != nil {
			return domain.CommandResponse{}, err
		}
		return domain.CommandResponse{Message: fmt.Sprintf("parent attached · %s", updated.ParentArtifactIDs[len(updated.ParentArtifactIDs)-1]), Recording: &updated}, nil

	case "USE":
		if len(fields) < 2 {
			return domain.CommandResponse{}, fmt.Errorf("usage: USE kind:name [WITH kind:name ...] [--profile=name]: %w", store.ErrConflict)
		}
		use := domain.UseRequest{Operator: actor, Profile: "default"}
		flagStart := 2
		if strings.Contains(fields[1], ":") {
			use.Selector = strings.ToLower(fields[1])
		} else {
			switch strings.ToLower(fields[1]) {
			case "tool", "credential", "session":
				if len(fields) < 3 {
					return domain.CommandResponse{}, fmt.Errorf("usage: USE %s:<name>: %w", fields[1], store.ErrConflict)
				}
				use.Selector = strings.ToLower(fields[1]) + ":" + fields[2]
				flagStart = 3
			default:
				// Compatibility with the original shorthand; canonical output always
				// uses the explicit selector.
				use.Selector = "tool:" + fields[1]
			}
		}
		withSelectors, flags, err := parseUseTail(fields[flagStart:])
		if err != nil {
			return domain.CommandResponse{}, err
		}
		use.WithSelectors = withSelectors
		if value := flags["profile"]; value != "" {
			use.Profile = value
		}
		composition, err := s.useCapsule(ctx, use)
		if err != nil {
			return domain.CommandResponse{}, err
		}
		message := fmt.Sprintf("composition %s · USE %s · operator=%s · entry=%s", composition.ID, composition.Selector, composition.Operator, composition.EntryArtifactID)
		if len(composition.WithSelectors) > 0 {
			message += " · WITH " + strings.Join(composition.WithSelectors, " WITH ")
		}
		if len(composition.Enabled) > 0 {
			message += " · ENABLED=" + enablementNames(composition.Enabled)
		}
		if len(composition.Warnings) > 0 {
			message += "\nwarning: " + strings.Join(composition.Warnings, "; ")
		}
		if composition.Runtime != nil && composition.Runtime.AttachCommand != "" {
			message += "\nready: " + composition.Runtime.AttachCommand
		}
		return domain.CommandResponse{Message: message, Composition: &composition}, nil

	case "ACP":
		if len(fields) < 2 || strings.ToUpper(fields[1]) != "PROBE" || len(fields) > 3 {
			return domain.CommandResponse{}, fmt.Errorf("usage: ACP PROBE [composition-id]: %w", store.ErrConflict)
		}
		compositionID := ""
		if len(fields) == 3 {
			compositionID = fields[2]
		} else {
			for _, composition := range s.store.Snapshot().Compositions {
				if composition.Operator != normalizeOperator(actor) || composition.Runtime == nil || composition.Runtime.Status == "stopped" {
					continue
				}
				for _, enabled := range composition.Enabled {
					if enabled.Name == "acp" {
						compositionID = composition.ID
					}
				}
			}
		}
		if compositionID == "" {
			return domain.CommandResponse{}, fmt.Errorf("no running ACP-enabled composition for %s: %w", actor, store.ErrNotFound)
		}
		probe, err := s.probeACP(ctx, compositionID, actor)
		if err != nil {
			return domain.CommandResponse{}, err
		}
		pretty, _ := json.MarshalIndent(probe.Handshake, "", "  ")
		return domain.CommandResponse{Message: "ACP v1 handshake succeeded · " + probe.Enablement.Command, Output: string(pretty)}, nil

	case "STOP":
		if len(fields) < 2 || strings.ToUpper(fields[1]) != "USE" || len(fields) > 3 {
			return domain.CommandResponse{}, fmt.Errorf("usage: STOP USE [composition-id]: %w", store.ErrConflict)
		}
		compositionID := ""
		if len(fields) == 3 {
			compositionID = fields[2]
		} else {
			for _, composition := range s.store.Snapshot().Compositions {
				if composition.Operator == normalizeOperator(actor) && composition.Runtime != nil && composition.Runtime.Status != "stopped" {
					compositionID = composition.ID
					break
				}
			}
		}
		if compositionID == "" {
			return domain.CommandResponse{}, fmt.Errorf("no running composition for %s: %w", actor, store.ErrNotFound)
		}
		composition, err := s.stopCapsule(ctx, compositionID, actor)
		if err != nil {
			return domain.CommandResponse{}, err
		}
		return domain.CommandResponse{Message: "stopped composition " + composition.ID, Composition: &composition}, nil

	case "LIST":
		kind := ""
		if len(fields) > 1 {
			kind = strings.ToLower(fields[1])
		}
		artifacts := s.store.Snapshot().Artifacts
		filtered := make([]domain.Artifact, 0, len(artifacts))
		for _, artifact := range artifacts {
			if artifact.SupersededBy != "" {
				continue
			}
			if kind == "" || string(artifact.Kind) == kind {
				filtered = append(filtered, artifact)
			}
		}
		return domain.CommandResponse{Message: fmt.Sprintf("%d artifact(s)", len(filtered)), Artifacts: filtered}, nil
	}

	recording, err := s.store.OpenRecording(actor)
	if err != nil {
		return domain.CommandResponse{}, fmt.Errorf("unknown Spin command %q; start RECORD before entering shell commands: %w", fields[0], store.ErrConflict)
	}
	updated, execution, err := s.executeRecordingCommand(ctx, recording.ID, domain.ExecuteRecordingCommandRequest{Actor: actor, Input: line})
	if err != nil {
		return domain.CommandResponse{}, err
	}
	return domain.CommandResponse{
		Message:   fmt.Sprintf("executed command %d in %s:%s · exit=%d", len(updated.Commands), updated.Kind, updated.Name, execution.ExitCode),
		Output:    execution.Output,
		ExitCode:  &execution.ExitCode,
		Recording: &updated,
	}, nil
}

func commandFlags(fields []string) map[string]string {
	flags := map[string]string{}
	for i := 0; i < len(fields); i++ {
		field := fields[i]
		if !strings.HasPrefix(field, "--") {
			continue
		}
		field = strings.TrimPrefix(field, "--")
		if key, value, ok := strings.Cut(field, "="); ok {
			flags[strings.ToLower(key)] = value
			continue
		}
		if i+1 < len(fields) && !strings.HasPrefix(fields[i+1], "--") {
			flags[strings.ToLower(field)] = fields[i+1]
			i++
			continue
		}
		flags[strings.ToLower(field)] = "true"
	}
	return flags
}

func parseUseTail(fields []string) ([]string, map[string]string, error) {
	withSelectors := []string{}
	flagFields := []string{}
	for i := 0; i < len(fields); {
		field := fields[i]
		if strings.EqualFold(field, "WITH") {
			if i+1 >= len(fields) || strings.HasPrefix(fields[i+1], "--") || strings.EqualFold(fields[i+1], "WITH") {
				return nil, nil, fmt.Errorf("WITH requires kind:name: %w", store.ErrConflict)
			}
			selector := strings.ToLower(strings.TrimSpace(fields[i+1]))
			kind, name, ok := strings.Cut(selector, ":")
			if !ok || kind == "" || name == "" {
				return nil, nil, fmt.Errorf("WITH requires kind:name, got %q: %w", fields[i+1], store.ErrConflict)
			}
			withSelectors = append(withSelectors, selector)
			i += 2
			continue
		}
		if !strings.HasPrefix(field, "--") {
			return nil, nil, fmt.Errorf("unexpected USE argument %q; expected WITH kind:name or --flag: %w", field, store.ErrConflict)
		}
		flagFields = append(flagFields, field)
		i++
		if !strings.Contains(field, "=") && i < len(fields) && !strings.HasPrefix(fields[i], "--") && !strings.EqualFold(fields[i], "WITH") {
			flagFields = append(flagFields, fields[i])
			i++
		}
	}
	return uniqueCommandStrings(withSelectors), commandFlags(flagFields), nil
}

func uniqueCommandStrings(values []string) []string {
	out := make([]string, 0, len(values))
	seen := map[string]bool{}
	for _, value := range values {
		if !seen[value] {
			seen[value] = true
			out = append(out, value)
		}
	}
	return out
}

func recordTarget(fields []string) (domain.ArtifactKind, string, int, error) {
	if len(fields) < 2 {
		return "", "", 0, fmt.Errorf("usage: RECORD kind:name: %w", store.ErrConflict)
	}
	if kind, name, ok := strings.Cut(strings.ToLower(fields[1]), ":"); ok && kind != "" && name != "" {
		return domain.ArtifactKind(kind), name, 2, nil
	}
	if len(fields) >= 3 && !strings.HasPrefix(fields[2], "--") {
		return domain.ArtifactKind(strings.ToLower(fields[1])), strings.ToLower(fields[2]), 3, nil
	}
	return "", "", 0, fmt.Errorf("target must be kind:name, got %q: %w", fields[1], store.ErrConflict)
}

func commandEnablements(flags map[string]string) ([]domain.Enablement, error) {
	raw := strings.TrimSpace(flags["enable"])
	if raw == "" {
		raw = strings.TrimSpace(flags["enables"])
	}
	if raw == "" {
		if flags["command"] != "" || flags["transport"] != "" || flags["protocol-version"] != "" {
			return nil, fmt.Errorf("--command/--transport/--protocol-version require --enable: %w", store.ErrConflict)
		}
		return nil, nil
	}
	names := strings.Split(raw, ",")
	if len(names) > 1 && flags["command"] != "" {
		return nil, fmt.Errorf("--command is ambiguous with multiple enabled capabilities: %w", store.ErrConflict)
	}
	protocolVersion := 0
	if rawVersion := strings.TrimSpace(flags["protocol-version"]); rawVersion != "" {
		parsed, err := strconv.Atoi(rawVersion)
		if err != nil || parsed < 0 {
			return nil, fmt.Errorf("invalid --protocol-version %q: %w", rawVersion, store.ErrConflict)
		}
		protocolVersion = parsed
	}
	enables := make([]domain.Enablement, 0, len(names))
	for _, name := range names {
		name = strings.ToLower(strings.TrimSpace(name))
		if name == "" {
			continue
		}
		enables = append(enables, domain.Enablement{
			Name:            name,
			Command:         strings.TrimSpace(flags["command"]),
			Transport:       strings.ToLower(strings.TrimSpace(flags["transport"])),
			ProtocolVersion: protocolVersion,
		})
	}
	return enables, nil
}

func enablementNames(values []domain.Enablement) string {
	names := make([]string, 0, len(values))
	for _, value := range values {
		names = append(names, value.Name)
	}
	return strings.Join(names, ",")
}
