#!/usr/bin/env bash
# Every pattern in ci.yml's dorny/paths-filter groups must correspond to
# something that exists in the repository.
#
# The matcher the pinned action builds is case-sensitive: it passes only
# `{dot: true}` to picomatch, whose `nocase` default is false. A pattern that
# names a path with the wrong case therefore matches nothing, silently, and the
# lanes gated on that filter never run for the change it was meant to catch.
# That is exactly how `justfile` sat in the `rust` group while the repository
# file is `Justfile`, leaving a Justfile-only pull request with no rust, no
# desktop and no desktop-rust output - and so no compiled-flag shards, a
# skipped `Desktop` aggregate, and a green `CI Complete`.
set -euo pipefail

root=${BUZZ_PATH_FILTER_CONTRACT_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}

ruby - "$root" <<'RUBY'
require "pathname"
require "yaml"

root = Pathname(ARGV[0])
workflow_path = root / ".github" / "workflows" / "ci.yml"
abort "#{workflow_path}: missing" unless workflow_path.file?

workflow = begin
  YAML.safe_load(workflow_path.binread.force_encoding("UTF-8"), aliases: true)
rescue Psych::Exception => error
  abort "#{workflow_path}: invalid workflow YAML: #{error.message}"
end

steps = workflow.dig("jobs", "changes", "steps")
abort "ci.yml: the changes job has no steps" unless steps.is_a?(Array)
step = steps.find { |candidate| candidate.is_a?(Hash) && candidate["id"] == "filter" }
abort "ci.yml: the changes job has no step with id 'filter'" if step.nil?

filters = begin
  YAML.safe_load(step.dig("with", "filters").to_s)
rescue Psych::Exception => error
  abort "ci.yml: paths-filter 'filters' is not valid YAML: #{error.message}"
end
abort "ci.yml: paths-filter 'filters' is not a mapping" unless filters.is_a?(Hash)
abort "ci.yml: paths-filter 'filters' is empty" if filters.empty?

tracked = Dir.chdir(root) { IO.popen(["git", "ls-files", "-z"], &:read) }.split("\0")
abort "git ls-files reported no tracked files" if tracked.empty?
tracked_set = {}
tracked.each { |path| tracked_set[path] = true }

GLOB_CHARS = /[*?\[\]{}]/

# A literal pattern must name a tracked path exactly. A glob is checked
# permissively - `**` collapsed to `*` and separators allowed to match - so
# this only ever fails a glob that corresponds to nothing at all.
literal_miss = lambda do |pattern|
  return nil if tracked_set.key?(pattern)
  near = tracked.find { |path| path.casecmp?(pattern) }
  near ? "case mismatch: the repository has '#{near}'" : "no such tracked path"
end

glob_miss = lambda do |pattern|
  collapsed = pattern.gsub("**", "*")
  matched = tracked.any? { |path| File.fnmatch?(collapsed, path, File::FNM_DOTMATCH) }
  matched ? nil : "matches no tracked file"
end

check = lambda do |group, pattern|
  return nil if pattern.start_with?("!") # exclusions may legitimately match nothing
  reason = pattern.match?(GLOB_CHARS) ? glob_miss.call(pattern) : literal_miss.call(pattern)
  reason && "#{group}: '#{pattern}' - #{reason}"
end

# Self-test first: a contract that cannot fail is not a contract.
sentinel = "scripts/__path_filter_contract_sentinel__"
abort "self-test: sentinel path unexpectedly tracked" if tracked_set.key?(sentinel)
abort "self-test: an absent literal path was not rejected" if check.call("self-test", sentinel).nil?

case_probe = tracked.find do |path|
  path.match?(/[A-Z]/) && !tracked_set.key?(path.downcase)
end
if case_probe
  verdict = check.call("self-test", case_probe.downcase)
  abort "self-test: '#{case_probe.downcase}' was not rejected against '#{case_probe}'" if verdict.nil?
  unless verdict.include?("case mismatch")
    abort "self-test: case mismatch against '#{case_probe}' was not reported as one"
  end
end

failures = []
filters.each do |group, patterns|
  Array(patterns).each { |pattern| failures << check.call(group, pattern.to_s) }
end
failures.compact!

unless failures.empty?
  warn "ci.yml paths-filter patterns that match nothing in the repository:"
  failures.each { |failure| warn "  #{failure}" }
  warn ""
  warn "A dead pattern silently disables every lane gated on its filter."
  exit 1
end

count = filters.values.sum { |patterns| Array(patterns).size }
puts "path filter contract: #{count} patterns across #{filters.size} filter groups all resolve"
RUBY
