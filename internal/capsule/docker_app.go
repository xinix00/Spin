//go:build !tamago

package capsule

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"time"

	"easyacp/internal/domain"
)

// App services run next to a Session's capsule so a person can test the
// app: one container per service, all on one Docker network named after the
// Session so they reach each other by service name. A service with Run uses
// the Session's own image on the Session's workspace volume; a service with
// Image is a ready-made dependency. Secrets come from env files on this
// host (EnvDir/<name>.env) and never pass through the control plane. The
// repository's own host entries and host.docker.internal are added so a
// database the app reaches by name resolves inside the container.

const appEnvSuffix = ".env"

func appNetworkName(sessionID string) string { return runtimeName("spin-app", sessionID) }
func appContainerName(sessionID, service string) string {
	return runtimeName("spin-app", sessionID) + "-" + safeName(service)
}

// StartAppServices (re)starts every service of the recipe. An existing
// container of the same service is replaced, so a restart after a rejected
// attempt picks up the new workspace.
func (d *Docker) StartAppServices(ctx context.Context, runtime domain.CapsuleRuntime, sessionID string, services []domain.AppService, hosts []string) ([]domain.AppServiceRuntime, error) {
	if runtime.Driver != "docker" || runtime.BaseRef == "" {
		return nil, errors.New("session has no Docker capsule to run the app in")
	}
	if len(services) == 0 {
		return nil, errors.New("the repository has no app services configured")
	}
	network := appNetworkName(sessionID)
	if _, _, err := d.run(ctx, "network", "inspect", network); err != nil {
		if _, err := d.control(ctx, "network", "create", "--label", "spin.managed=true", "--label", "spin.session_id="+sessionID, network); err != nil {
			return nil, fmt.Errorf("create app network: %w", err)
		}
	}
	results := make([]domain.AppServiceRuntime, 0, len(services))
	for _, service := range services {
		result := d.startAppService(ctx, runtime, sessionID, network, service, hosts)
		results = append(results, result)
		if result.Error != "" {
			continue
		}
		// Give a dependency a moment to come up before the next service
		// tries to reach it.
		if service.Image != "" && len(service.Ports) > 0 {
			d.awaitPort(ctx, result, 15*time.Second)
		}
	}
	return results, nil
}

func (d *Docker) startAppService(ctx context.Context, runtime domain.CapsuleRuntime, sessionID, network string, service domain.AppService, hosts []string) domain.AppServiceRuntime {
	name := appContainerName(sessionID, service.Name)
	result := domain.AppServiceRuntime{Service: service.Name, Status: "starting", Host: d.advertiseHost}
	_ = d.removeContainer(ctx, name)
	args := []string{
		"run", "-d", "--name", name,
		"--label", "spin.managed=true",
		"--label", "spin.kind=app",
		"--label", "spin.session_id=" + sessionID,
		"--label", "spin.service=" + service.Name,
		"--network", network,
		"--network-alias", safeName(service.Name),
		"--add-host", "host.docker.internal:host-gateway",
	}
	for _, entry := range hosts {
		args = append(args, "--add-host", entry)
	}
	if service.Env != "" {
		envFile := filepath.Join(d.envDir, safeName(service.Env)+appEnvSuffix)
		if _, err := os.Stat(envFile); err != nil {
			result.Status, result.Error = "error", fmt.Sprintf("env file %s is missing on runner %s", envFile, d.advertiseHost)
			return result
		}
		args = append(args, "--env-file", envFile)
	}
	for _, port := range service.Ports {
		args = append(args, "-p", "0:"+strconv.Itoa(port))
	}
	if service.Image != "" {
		args = append(args, service.Image)
	} else {
		if runtime.WorkspaceRef != "" {
			args = append(args, "--mount", "type=volume,src="+runtime.WorkspaceRef+",dst=/workspace")
		}
		script := make([]string, 0, len(service.Prepare)+1)
		for _, command := range service.Prepare {
			if strings.TrimSpace(command) != "" {
				script = append(script, strings.TrimSpace(command))
			}
		}
		script = append(script, "exec "+strings.TrimSpace(service.Run))
		args = append(args, "--workdir", "/workspace", "--entrypoint", "sh", runtime.BaseRef, "-lc", strings.Join(script, " && "))
	}
	if _, err := d.control(ctx, args...); err != nil {
		result.Status, result.Error = "error", err.Error()
		return result
	}
	id, err := d.containerID(ctx, name)
	if err != nil {
		result.Status, result.Error = "error", err.Error()
		return result
	}
	result.ContainerID = id
	result.Ports = d.publishedPorts(ctx, id, service.Ports)
	now := time.Now().UTC()
	result.StartedAt = &now
	result.Status = "running"
	return result
}

// StopAppServices removes every service container and the network of a
// Session; a Session without any is fine.
func (d *Docker) StopAppServices(ctx context.Context, sessionID string) error {
	containers, err := d.appContainers(ctx, sessionID)
	if err != nil {
		return err
	}
	var errs []error
	for _, container := range containers {
		if err := d.removeContainer(ctx, container.ID); err != nil {
			errs = append(errs, err)
		}
	}
	if output, code, err := d.run(ctx, "network", "rm", appNetworkName(sessionID)); code != 0 && err != nil && !strings.Contains(output, "not found") && !strings.Contains(output, "No such network") {
		errs = append(errs, fmt.Errorf("remove app network: %s", strings.TrimSpace(output)))
	}
	return errors.Join(errs...)
}

type appContainer struct {
	ID      string
	Service string
	State   string
	Status  string
	Created time.Time
}

func (d *Docker) appContainers(ctx context.Context, sessionID string) ([]appContainer, error) {
	output, err := d.control(ctx, "ps", "-a", "--no-trunc",
		"--filter", "label=spin.kind=app", "--filter", "label=spin.session_id="+sessionID,
		"--format", "{{.ID}}\t{{.Label \"spin.service\"}}\t{{.State}}\t{{.Status}}\t{{.CreatedAt}}")
	if err != nil {
		return nil, err
	}
	var containers []appContainer
	scanner := bufio.NewScanner(strings.NewReader(output))
	for scanner.Scan() {
		fields := strings.Split(strings.TrimSpace(scanner.Text()), "\t")
		if len(fields) < 4 || fields[0] == "" {
			continue
		}
		container := appContainer{ID: fields[0], Service: fields[1], State: fields[2], Status: fields[3]}
		if len(fields) > 4 {
			if created, err := time.Parse("2006-01-02 15:04:05 -0700 MST", fields[4]); err == nil {
				container.Created = created.UTC()
			}
		}
		containers = append(containers, container)
	}
	sort.Slice(containers, func(i, j int) bool { return containers[i].Service < containers[j].Service })
	return containers, nil
}

// AppServiceStatus reports every service container of a Session with its
// published ports and whether the first one answers on this host.
func (d *Docker) AppServiceStatus(ctx context.Context, sessionID string) ([]domain.AppServiceRuntime, error) {
	containers, err := d.appContainers(ctx, sessionID)
	if err != nil {
		return nil, err
	}
	results := make([]domain.AppServiceRuntime, 0, len(containers))
	for _, container := range containers {
		result := domain.AppServiceRuntime{Service: container.Service, ContainerID: container.ID, Host: d.advertiseHost, Status: container.State}
		if !container.Created.IsZero() {
			created := container.Created
			result.StartedAt = &created
		}
		if container.State == "running" {
			result.Ports = d.publishedPorts(ctx, container.ID, nil)
			result.Reachable = d.reachable(result, 700*time.Millisecond)
		} else {
			result.Error = container.Status
		}
		results = append(results, result)
	}
	return results, nil
}

// AppServiceLogs returns the tail of a service's output.
func (d *Docker) AppServiceLogs(ctx context.Context, sessionID, service string, tail int) (string, error) {
	if tail <= 0 {
		tail = 200
	}
	output, _, err := d.run(ctx, "logs", "--tail", strconv.Itoa(tail), "--timestamps", appContainerName(sessionID, service))
	if err != nil && strings.TrimSpace(output) == "" {
		return "", fmt.Errorf("logs of %s: %w", service, err)
	}
	return output, nil
}

// publishedPorts maps the container ports to the host ports Docker chose.
func (d *Docker) publishedPorts(ctx context.Context, containerID string, wanted []int) map[string]int {
	output, _, err := d.run(ctx, "inspect", "--format", "{{json .NetworkSettings.Ports}}", containerID)
	if err != nil {
		return nil
	}
	var bindings map[string][]struct {
		HostPort string `json:"HostPort"`
	}
	if json.Unmarshal([]byte(strings.TrimSpace(output)), &bindings) != nil {
		return nil
	}
	ports := map[string]int{}
	for key, hostBindings := range bindings {
		containerPort := strings.TrimSuffix(key, "/tcp")
		for _, binding := range hostBindings {
			if hostPort, err := strconv.Atoi(binding.HostPort); err == nil && hostPort > 0 {
				ports[containerPort] = hostPort
				break
			}
		}
	}
	if len(ports) == 0 {
		return nil
	}
	return ports
}

func (d *Docker) reachable(result domain.AppServiceRuntime, timeout time.Duration) bool {
	for _, hostPort := range result.Ports {
		connection, err := net.DialTimeout("tcp", net.JoinHostPort("127.0.0.1", strconv.Itoa(hostPort)), timeout)
		if err == nil {
			_ = connection.Close()
			return true
		}
	}
	return false
}

func (d *Docker) awaitPort(ctx context.Context, result domain.AppServiceRuntime, limit time.Duration) {
	deadline := time.Now().Add(limit)
	for time.Now().Before(deadline) && ctx.Err() == nil {
		if d.reachable(result, 500*time.Millisecond) {
			return
		}
		time.Sleep(500 * time.Millisecond)
	}
}

// AdvertiseHost is the address people use to reach published ports: the
// configured one, else the address this machine uses towards the network.
func AdvertiseHost(configured string) string {
	if configured = strings.TrimSpace(configured); configured != "" {
		return configured
	}
	connection, err := net.Dial("udp", "192.0.2.1:9")
	if err != nil {
		return "127.0.0.1"
	}
	defer connection.Close()
	if address, ok := connection.LocalAddr().(*net.UDPAddr); ok && address.IP != nil {
		return address.IP.String()
	}
	return "127.0.0.1"
}
