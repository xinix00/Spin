package replica

import (
	"errors"
	"os"
	"strings"
	"time"
)

// Config is where the replicas go. Every value comes from the environment:
// SPIN_S3_ENDPOINT, SPIN_S3_BUCKET, SPIN_S3_ACCESS_KEY, SPIN_S3_SECRET_KEY,
// and optionally SPIN_S3_REGION (default auto) and SPIN_S3_PREFIX (default
// spin). Replication is required; SPIN_REPLICATION=off switches it off for a
// development server only.
type Config struct {
	Endpoint  string
	Bucket    string
	Region    string
	AccessKey string
	SecretKey string
	Prefix    string
	// Interval is how often changed pages travel; SegmentBytes bounds one
	// segment (and the memory a sync pass holds).
	Interval     time.Duration
	SegmentBytes int
}

func (c Config) withDefaults() Config {
	if c.Prefix == "" {
		c.Prefix = "spin"
	}
	if c.Interval <= 0 {
		c.Interval = 15 * time.Second
	}
	if c.SegmentBytes <= 0 {
		c.SegmentBytes = 16 << 20
	}
	return c
}

// ConfigFromEnvironment reads the S3 settings. The bool is false when
// replication is switched off explicitly; an incomplete configuration is an
// error, because a Spin without replicas is not a Spin.
func ConfigFromEnvironment(get func(string) string) (Config, bool, error) {
	value := func(name string) string { return strings.TrimSpace(get(name)) }
	if strings.EqualFold(value("SPIN_REPLICATION"), "off") {
		return Config{}, false, nil
	}
	config := Config{
		Endpoint: value("SPIN_S3_ENDPOINT"), Bucket: value("SPIN_S3_BUCKET"), Region: value("SPIN_S3_REGION"),
		AccessKey: value("SPIN_S3_ACCESS_KEY"), SecretKey: value("SPIN_S3_SECRET_KEY"), Prefix: strings.Trim(value("SPIN_S3_PREFIX"), "/"),
	}
	var missing []string
	for name, present := range map[string]string{"SPIN_S3_ENDPOINT": config.Endpoint, "SPIN_S3_BUCKET": config.Bucket, "SPIN_S3_ACCESS_KEY": config.AccessKey, "SPIN_S3_SECRET_KEY": config.SecretKey} {
		if present == "" {
			missing = append(missing, name)
		}
	}
	if len(missing) > 0 {
		return Config{}, true, errors.New("replication needs " + strings.Join(sorted(missing), ", ") + " (or SPIN_REPLICATION=off for a development server)")
	}
	return config.withDefaults(), true, nil
}

func sorted(values []string) []string {
	for i := 1; i < len(values); i++ {
		for j := i; j > 0 && values[j] < values[j-1]; j-- {
			values[j], values[j-1] = values[j-1], values[j]
		}
	}
	return values
}

// EnvironmentGetter adapts os.Getenv and HopOS' app.Env.
var EnvironmentGetter = os.Getenv
