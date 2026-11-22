#!/usr/bin/env python3
"""Run scoped negative controls on copied actual Gateway sources; never edit checkout."""
import argparse, difflib, hashlib, json, pathlib, shutil, subprocess, tempfile, time

ROOT = pathlib.Path(__file__).resolve().parents[1]
NORTH = 'crates/gateway/src/firmware/northbound.rs'
LINK = 'crates/gateway/src/firmware/north_link.rs'
EVENTS = 'crates/gateway/src/firmware/role_events.rs'

def remove_writer_branch(text, nested):
    needle = 'w = &mut writer => {'
    starts = [i for i in range(len(text)) if text.startswith(needle, i)]
    assert len(starts) == 2
    start = starts[0 if nested else 1]
    brace = text.index('{', start)
    depth, end = 1, brace + 1
    while depth:
        depth += (text[end] == '{') - (text[end] == '}')
        end += 1
    return text[:start] + text[end:]

def mutate_observation(text, nested):
    needle = '_=tx.failed()=>break Err(tx.reason().into()),'
    starts = [i for i in range(len(text)) if text.startswith(needle, i)]
    assert len(starts) == 2
    start = starts[1 if nested else 0]
    text = text[:start] + text[start + len(needle):]
    return remove_writer_branch(text, nested)

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--out', type=pathlib.Path)
    args = parser.parse_args()
    out = args.out or pathlib.Path(tempfile.mkdtemp(prefix='luna-northbound-mutants-'))
    out.mkdir(parents=True, exist_ok=True)
    source = out / 'source'
    source.mkdir()
    paths = sorted(set(subprocess.check_output(['git', 'ls-files', '--cached', '--others', '--exclude-standard'], cwd=ROOT, text=True).splitlines()))
    manifest = []
    for name in paths:
        origin, destination = ROOT/name, source/name
        if not origin.is_file():
            continue
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(origin, destination)
        manifest.append({'path': name, 'sha256': hashlib.sha256(origin.read_bytes()).hexdigest()})
    (out/'source-manifest.json').write_text(json.dumps(manifest, indent=2)+'\n')
    metadata = {'base': subprocess.check_output(['git','rev-parse','HEAD'],cwd=ROOT,text=True).strip(), 'source_manifest_sha256': hashlib.sha256(json.dumps(manifest,sort_keys=True,separators=(',',':')).encode()).hexdigest(), 'cases': []}
    cases = [
        ('critical_count_guard', NORTH, lambda s: s.replace('if q.retained_count >= CRITICAL_COUNT_CAP {','if false {',1), ['--test','firmware_contract','normal_core_command_burst_exhausts_critical_count_and_reconnects']),
        ('critical_byte_guard', NORTH, lambda s: s.replace('if entry.bytes.len() > CRITICAL_BYTE_CAP.saturating_sub(q.retained_bytes) {','if false {',1), ['--test','firmware_contract','normal_core_command_burst_exhausts_critical_bytes_and_reconnects']),
        ('frame_deadline', NORTH, lambda s: s.replace('pub const WRITE_DEADLINE_MS: u64 = 1000;','pub const WRITE_DEADLINE_MS: u64 = 60_000;',1), ['--lib','production_writer_deadline_bounds_pending_and_trickle_but_not_idle']),
        ('outer_failure_observation', LINK, lambda s: mutate_observation(s,False), ['--lib','fatal_latch_ends_real_outer_reader_select']),
        ('nested_failure_observation', LINK, lambda s: mutate_observation(s,True), ['--lib','fatal_latch_ends_real_nested_decode_capacity_select']),
        ('critical_biased_fairness', NORTH, lambda s: s.replace('if q.round_lane == 0 && q.critical_burst < 8 {','if true {',1), ['--lib','telemetry_latest_slots_audio_drop_and_critical_fairness_are_independent']),
        ('old_stream_done_identity', EVENTS, lambda s: s.replace('if sh.stamp(chan) != stamp {','if false {',1), ['--test','firmware_contract','actual_socket_same_task_reconnect_does_not_attribute_old_dac_completion_to_fresh_play']),
        ('actual_socket_write_timeout', NORTH, lambda s: s.replace('tokio::time::timeout(Duration::from_millis(WRITE_DEADLINE_MS), async {','tokio::time::timeout(Duration::from_millis(60_000), async {',1), ['--test','firmware_contract','actual_socket_stalled_writer_has_fixed_deadline_and_bounded_cleanup']),
    ]
    env = dict(__import__('os').environ, CARGO_TARGET_DIR=str(out/'target'))
    for name, path, mutation, tests in cases:
        original = (source/path).read_text()
        command = ['cargo','test','-p','home-mate-device-gateway',*tests,'--locked','--','--nocapture','--test-threads=1']
        positive = subprocess.run(command, cwd=source, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=180)
        (out/(name+'.positive.log')).write_text(positive.stdout)
        if positive.returncode:
            raise RuntimeError(f'positive control failed: {name}')
        changed = mutation(original)
        assert changed != original, name
        patch = ''.join(difflib.unified_diff(original.splitlines(keepends=True),changed.splitlines(keepends=True),fromfile='a/'+path,tofile='b/'+path))
        (out/(name+'.patch')).write_text(patch)
        (source/path).write_text(changed)
        try:
            negative = subprocess.run(command, cwd=source, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=180)
        finally:
            (source/path).write_text(original)
        (out/(name+'.negative.log')).write_text(negative.stdout)
        # A compiler error is not a negative control; require the actual test assertion.
        expected = 'test result: FAILED.' in negative.stdout and 'panicked at' in negative.stdout
        row = {'name':name,'command':command,'positive_exit':positive.returncode,'negative_exit':negative.returncode,'negative_assertion_observed':expected,'mutated_source_sha256':hashlib.sha256(changed.encode()).hexdigest(),'patch_sha256':hashlib.sha256(patch.encode()).hexdigest()}
        metadata['cases'].append(row)
        (out/'report.json').write_text(json.dumps(metadata,indent=2)+'\n')
        if negative.returncode == 0 or not expected:
            raise RuntimeError(f'mutant did not fail its production-path test assertion: {name}')
        print(f'PASS negative control {name}', flush=True)
    print(out, flush=True)

if __name__ == '__main__':
    main()
