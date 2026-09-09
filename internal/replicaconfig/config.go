package replicaconfig

import (
	"easyacp/replica"
	"errors"
	"fmt"
	"strings"
	"time"
)

// FromEnvironment reads the S3 settings. The bool is false when
// replication is switched off explicitly; an incomplete configuration is an
// error, because a Spin without replicas is not a Spin.
func FromEnvironment(get func(string) string) (replica.Config, bool, error) {
	value := func(name string) string { return strings.TrimSpace(get(name)) }
	if strings.EqualFold(value("SPIN_REPLICATION"), "off") {
		return replica.Config{}, false, nil
	}
	config := replica.Config{
		Endpoint: value("SPIN_S3_ENDPOINT"), Bucket: value("SPIN_S3_BUCKET"), Region: value("SPIN_S3_REGION"),
		AccessKey: value("SPIN_S3_ACCESS_KEY"), SecretKey: value("SPIN_S3_SECRET_KEY"), Prefix: strings.Trim(value("SPIN_S3_PREFIX"), "/"),
	}
	if config.Prefix == "" {
		config.Prefix = "spin"
	}
	var missing []string
	for name, present := range map[string]string{"SPIN_S3_ENDPOINT": config.Endpoint, "SPIN_S3_BUCKET": config.Bucket, "SPIN_S3_ACCESS_KEY": config.AccessKey, "SPIN_S3_SECRET_KEY": config.SecretKey} {
		if present == "" {
			missing = append(missing, name)
		}
	}
	if len(missing) > 0 {
		return replica.Config{}, true, errors.New("replication needs " + strings.Join(sorted(missing), ", ") + " (or SPIN_REPLICATION=off for a development server)")
	}
	schedule, err := replica.ParseSchedule(value("SPIN_REPLICA_SCHEDULE"))
	if err != nil {
		return replica.Config{}, true, fmt.Errorf("SPIN_REPLICA_SCHEDULE: %w", err)
	}
	config.Schedule = schedule
	for name, target := range map[string]*time.Duration{"SPIN_REPLICA_GENERATION": &config.Generation, "SPIN_REPLICA_RETENTION": &config.Retention} {
		if raw := value(name); raw != "" {
			duration, err := time.ParseDuration(raw)
			if err != nil || duration < time.Hour {
				return replica.Config{}, true, fmt.Errorf("%s: a duration of at least an hour", name)
			}
			*target = duration
		}
	}
	return config, true, nil
}

func sorted(values []string) []string {
	for i := 1; i < len(values); i++ {
		for j := i; j > 0 && values[j] < values[j-1]; j-- {
			values[j], values[j-1] = values[j-1], values[j]
		}
	}
	return values
}
