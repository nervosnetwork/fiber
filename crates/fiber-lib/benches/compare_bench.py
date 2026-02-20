#!/usr/bin/env python3
import re
import sys

def parse_bench_file(filename):
    results = {}
    with open(filename, 'r') as f:
        for line in f:
            # bencher format: test bench_name ... bench: 1,234 ns/iter (+/- 56)
            match = re.search(r'test (\S+) .* bench:\s+([\d,]+) ns/iter', line)
            if match:
                name = match.group(1)
                time = int(match.group(2).replace(',', ''))
                results[name] = time
    return results

baseline = parse_bench_file('baseline.txt')
current = parse_bench_file('current.txt')

print("Baseline benchmarks:")
for name, time in baseline.items():
    print(f"  {name}: {time:,} ns/iter")
print("Current benchmarks:")
for name, time in current.items():
    print(f"  {name}: {time:,} ns/iter")

regression_threshold = 0.25   # 25% regression threadold
improvement_threshold = 0.20  # 20% improvement threshold

regressions = []
improvements = []
no_change = []

for test_name in current:
    if test_name in baseline:
        change = (current[test_name] - baseline[test_name]) / baseline[test_name]

        if change > regression_threshold:
            regressions.append({
                'test': test_name,
                'change': change * 100,
                'baseline': baseline[test_name],
                'current': current[test_name]
            })
        elif change < -improvement_threshold:
            improvements.append({
                'test': test_name,
                'change': abs(change) * 100,
                'baseline': baseline[test_name],
                'current': current[test_name]
            })
        else:
            no_change.append({
                'test': test_name,
                'change': change * 100,
                'baseline': baseline[test_name],
                'current': current[test_name]
            })

print("\n" + "="*50)

if improvements:
    print("🚀 Performance improvements detected:")
    for imp in improvements:
        print(f"  {imp['test']}: {imp['change']:.2f}% faster")
        print(f"    Baseline: {imp['baseline']:,} ns/iter")
        print(f"    Current:  {imp['current']:,} ns/iter")
    print()

if regressions:
    print("❌ Performance regressions detected:")
    for reg in regressions:
        print(f"  {reg['test']}: {reg['change']:.2f}% slower")
        print(f"    Baseline: {reg['baseline']:,} ns/iter")
        print(f"    Current:  {reg['current']:,} ns/iter")
    print()

if no_change:
    print("➡️  No significant changes:")
    for nc in no_change:
        change_str = "faster" if nc['change'] < 0 else "slower"
        print(f"  {nc['test']}: {abs(nc['change']):.2f}% {change_str}")

print("="*50)
print(f"Summary: {len(improvements)} improvements, {len(regressions)} regressions, {len(no_change)} unchanged")

if regressions:
    print("\n❌ CI will fail due to performance regressions")
    sys.exit(1)
else:
    print("\n✅ No performance regressions detected")
    sys.exit(0)