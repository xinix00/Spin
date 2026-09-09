package replicaconfig

import "testing"

func TestEnvironmentConfiguration(t *testing.T) {
	values := map[string]string{}
	get := func(name string) string { return values[name] }
	if _, enabled, err := FromEnvironment(get); !enabled || err == nil {
		t.Fatal("missing required configuration accepted")
	}
	values["SPIN_REPLICATION"] = "off"
	if _, enabled, err := FromEnvironment(get); enabled || err != nil {
		t.Fatal("explicit development mode rejected")
	}
	delete(values, "SPIN_REPLICATION")
	for _, key := range []string{"SPIN_S3_ENDPOINT", "SPIN_S3_BUCKET", "SPIN_S3_ACCESS_KEY", "SPIN_S3_SECRET_KEY"} {
		values[key] = "test"
	}
	c, enabled, err := FromEnvironment(get)
	if err != nil || !enabled || c.Prefix != "spin" || len(c.Schedule) != 3 {
		t.Fatalf("config %+v, %v", c, err)
	}
	values["SPIN_REPLICA_SCHEDULE"] = "15m:2h,20m:1h"
	if _, _, err := FromEnvironment(get); err == nil {
		t.Fatal("non-nested windows accepted")
	}
	delete(values, "SPIN_REPLICA_SCHEDULE")
	values["SPIN_REPLICA_RETENTION"] = "1m"
	if _, _, err := FromEnvironment(get); err == nil {
		t.Fatal("invalid retention accepted")
	}
}
