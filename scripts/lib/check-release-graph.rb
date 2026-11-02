#!/usr/bin/env ruby
# frozen_string_literal: true

# Structural guard for the real-backend release gate.
#
# This is intentionally a graph check, not a YAML pretty-printer: it
# fails if a future edit lets the publish job run without the gate, if
# the gate loses its `always()` semantics, if the reusable suite stops
# running the one-command scripts, or if a Ceph/DRBD job stops targeting
# the documented self-hosted labels. ci.yaml runs it on every change.
#
# Usage: ruby scripts/lib/check-release-graph.rb release.yaml real-backends.yaml

require "yaml"

def load_workflow(path)
  data = YAML.load_file(path)
  raise "#{path}: top-level YAML is not a mapping" unless data.is_a?(Hash)
  raise "#{path}: missing jobs mapping" unless data["jobs"].is_a?(Hash)

  data
rescue Psych::SyntaxError => e
  warn "#{path}: invalid YAML: #{e.message}"
  exit 2
end

def needs(job, path, name)
  value = job["needs"]
  return [] if value.nil?
  return [value] if value.is_a?(String)
  return value if value.is_a?(Array)

  raise "#{path}: jobs.#{name}.needs must be a string or array"
end

def require_need(job, path, name, dependency)
  return if needs(job, path, name).include?(dependency)

  raise "#{path}: jobs.#{name}.needs must include #{dependency.inspect} (found #{needs(job, path, name).inspect})"
end

def require_key(hash, path, label, key)
  raise "#{path}: #{label} is missing #{key.inspect}" unless hash.key?(key)
end

def run_scripts(job)
  job.fetch("steps", []).filter_map { |step| step.is_a?(Hash) ? step["run"] : nil }.join("\n")
end

def assert(condition, message)
  raise message unless condition
end

if ARGV.length != 2
  warn "usage: #{$PROGRAM_NAME} release.yaml real-backends.yaml"
  exit 64
end

release_path, real_path = ARGV
release = load_workflow(release_path)
real = load_workflow(real_path)

release_jobs = release["jobs"]
real_jobs = real["jobs"]

# 1. The scope is the only source of the gate matrix.
scope = release_jobs["scope"] or raise "#{release_path}: missing jobs.scope"
assert(needs(scope, release_path, "scope").include?("guard"),
       "#{release_path}: jobs.scope must need guard")
scope_script = run_scripts(scope)
assert(scope_script.include?("release-scope.py check"),
       "#{release_path}: jobs.scope must validate release-scope.toml")
assert(scope_script.include?("release-scope.py matrix"),
       "#{release_path}: jobs.scope must emit the real-backends matrix")
assert(scope.dig("outputs", "supported_count"),
       "#{release_path}: jobs.scope must export supported_count")

# 2. The reusable suite is called once per supported suite, with fail-fast
#    off so evidence is collected per suite.
real_backends = release_jobs["real-backends"] or raise "#{release_path}: missing jobs.real-backends"
assert(real_backends["uses"] == "./.github/workflows/real-backends.yaml",
       "#{release_path}: jobs.real-backends must call ./.github/workflows/real-backends.yaml")
require_need(real_backends, release_path, "real-backends", "scope")
assert(real_backends.dig("strategy", "fail-fast") == false,
       "#{release_path}: jobs.real-backends strategy.fail-fast must be false")
assert(real_backends.to_s.include?("needs.scope.outputs.matrix"),
       "#{release_path}: jobs.real-backends matrix must come from jobs.scope.outputs.matrix")

# 3. The gate aggregates the matrix and cannot be skipped.
gate = release_jobs["real-backends-gate"] or raise "#{release_path}: missing jobs.real-backends-gate"
require_need(gate, release_path, "real-backends-gate", "scope")
require_need(gate, release_path, "real-backends-gate", "real-backends")
gate_if = gate["if"].to_s
assert(gate_if.include?("always()"),
       "#{release_path}: jobs.real-backends-gate must run with if: always()")
gate_script = run_scripts(gate)
assert(gate_script.include?("SUITE_RESULT") && gate_script.include?("SUPPORTED_COUNT"),
       "#{release_path}: jobs.real-backends-gate must inspect the suite and scope results")
assert(gate_script.include?('!= "success"'),
       "#{release_path}: jobs.real-backends-gate must fail a non-success suite result")

# 4. Publish (the tag-only `release` job) must need the gate directly,
#    and must consume generated scope/evidence rather than restating it.
publish = release_jobs["publish"] or raise "#{release_path}: missing jobs.publish"
require_need(publish, release_path, "publish", "real-backends-gate")
require_need(publish, release_path, "publish", "evidence-bundle")
require_need(publish, release_path, "publish", "scope")
publish_script = run_scripts(publish)
assert(publish_script.include?("gh release create"),
       "#{release_path}: jobs.publish must create the GitHub release")
assert(publish_script.include?("release-scope.py notes"),
       "#{release_path}: jobs.publish must render release-scope.py notes")
assert(publish_script.include?("real-backends-evidence"),
       "#{release_path}: jobs.publish must consume the real-backend evidence bundle")

# 5. The reusable workflow selects by the declared scope, gates the
#    selected job results, runs the one-command scripts on the documented
#    runners, and uploads evidence even after a failure.
reusable_scope = real_jobs["scope"] or raise "#{real_path}: missing jobs.scope"
assert(run_scripts(reusable_scope).include?("release-scope.py status"),
       "#{real_path}: jobs.scope must emit the supported/not-run suite selection")
reusable_gate = real_jobs["gate"] or raise "#{real_path}: missing jobs.gate"
%w[scope lvm ceph drbd].each do |dependency|
  require_need(reusable_gate, real_path, "gate", dependency)
end
assert(reusable_gate["if"].to_s.include?("always()"),
       "#{real_path}: jobs.gate must run with if: always()")

%w[lvm ceph drbd].each do |suite|
  job = real_jobs[suite] or raise "#{real_path}: missing jobs.#{suite}"
  require_need(job, real_path, suite, "scope")
  assert(run_scripts(job).include?("scripts/test-#{suite}.sh"),
         "#{real_path}: jobs.#{suite} must run scripts/test-#{suite}.sh")
  upload = job.fetch("steps", []).find do |step|
    step.is_a?(Hash) && step["uses"].to_s.include?("actions/upload-artifact")
  end
  assert(upload, "#{real_path}: jobs.#{suite} must upload evidence")
  assert(upload["if"].to_s.include?("always()"),
         "#{real_path}: jobs.#{suite} evidence upload must run with if: always()")
  assert(job.to_s.include?("retention-days"),
         "#{real_path}: jobs.#{suite} evidence must declare retention-days")
end

ceph_labels = real_jobs.dig("ceph", "runs-on")
drbd_labels = real_jobs.dig("drbd", "runs-on")
assert(ceph_labels.is_a?(Array) && (ceph_labels & %w[self-hosted volvisor-ceph]).length == 2,
       "#{real_path}: jobs.ceph.runs-on must target [self-hosted, ..., volvisor-ceph]")
assert(drbd_labels.is_a?(Array) && (drbd_labels & %w[self-hosted volvisor-drbd]).length == 2,
       "#{real_path}: jobs.drbd.runs-on must target [self-hosted, ..., volvisor-drbd]")
assert(real_jobs.dig("lvm", "runs-on") == "ubuntu-latest",
       "#{real_path}: jobs.lvm must run on ubuntu-latest")

puts "release gate graph OK: #{release_path} -> #{real_path}"
