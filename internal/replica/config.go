package replica

import (
	"errors"
	"fmt"
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
	// Schedule is how restore points thin out with age: level 1 merges the
	// raw segments into windows of its size and keeps them for its Keep,
	// level 2 merges level 1 windows, and so on. The last level is kept as
	// long as its generation. Default: 15m for 2h, 1h for 24h, 24h for 7d.
	Schedule []Level
	// Generation is how often a fresh snapshot starts a generation (also
	// when the changes outweigh the database); Retention is how long
	// generations stay, so generation starts are the coarsest points.
	// Default: a generation a week, kept four weeks.
	Generation time.Duration
	Retention  time.Duration
}

// Level is one tier of restore points: windows of Window, kept for Keep.
type Level struct {
	Window time.Duration
	Keep   time.Duration
}

// DefaultSchedule: quarter hours for two hours, hours for a day, days for
// a week; generations (weekly) carry the month.
var DefaultSchedule = []Level{
	{Window: 15 * time.Minute, Keep: 2 * time.Hour},
	{Window: time.Hour, Keep: 24 * time.Hour},
	{Window: 24 * time.Hour, Keep: 7 * 24 * time.Hour},
}

// ParseSchedule reads "15m:2h,1h:24h,24h:168h": window:keep per level,
// windows ascending and each a multiple of the previous.
func ParseSchedule(value string) ([]Level, error) {
	value = strings.TrimSpace(value)
	if value == "" {
		return append([]Level(nil), DefaultSchedule...), nil
	}
	var levels []Level
	for _, item := range strings.Split(value, ",") {
		window, keep, ok := strings.Cut(strings.TrimSpace(item), ":")
		if !ok {
			return nil, fmt.Errorf("schedule level %q must be window:keep", item)
		}
		level := Level{}
		var err error
		if level.Window, err = time.ParseDuration(strings.TrimSpace(window)); err != nil || level.Window < time.Minute {
			return nil, fmt.Errorf("schedule level %q: window must be a duration of at least a minute", item)
		}
		if level.Keep, err = time.ParseDuration(strings.TrimSpace(keep)); err != nil || level.Keep < level.Window {
			return nil, fmt.Errorf("schedule level %q: keep must be a duration of at least the window", item)
		}
		if len(levels) > 0 {
			previous := levels[len(levels)-1].Window
			if level.Window <= previous || level.Window%previous != 0 {
				return nil, fmt.Errorf("schedule level %q: window must be a multiple of the previous level's %s", item, previous)
			}
		}
		levels = append(levels, level)
	}
	return levels, nil
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
	if len(c.Schedule) == 0 {
		c.Schedule = append([]Level(nil), DefaultSchedule...)
	}
	if c.Generation <= 0 {
		c.Generation = 7 * 24 * time.Hour
	}
	if c.Retention <= 0 {
		c.Retention = 4 * 7 * 24 * time.Hour
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
	schedule, err := ParseSchedule(value("SPIN_REPLICA_SCHEDULE"))
	if err != nil {
		return Config{}, true, fmt.Errorf("SPIN_REPLICA_SCHEDULE: %w", err)
	}
	config.Schedule = schedule
	for name, target := range map[string]*time.Duration{"SPIN_REPLICA_GENERATION": &config.Generation, "SPIN_REPLICA_RETENTION": &config.Retention} {
		if raw := value(name); raw != "" {
			duration, err := time.ParseDuration(raw)
			if err != nil || duration < time.Hour {
				return Config{}, true, fmt.Errorf("%s: a duration of at least an hour", name)
			}
			*target = duration
		}
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
