#!/usr/bin/env python3
import os
import json
import sys
import glob

def check_regression(target_dir, threshold):
    regressions = []
    
    # Find all change/estimates.json files
    pattern = os.path.join(target_dir, "criterion", "*", "*", "change", "estimates.json")
    files = glob.glob(pattern)
    
    if not files:
        print("No benchmark comparison results found.")
        return 0

    print(f"Checking {len(files)} benchmarks for regression > {threshold*100}%")
    
    results = []
    for file_path in files:
        # Extract benchmark name from path
        parts = file_path.split(os.sep)
        bench_name = f"{parts[-4]}/{parts[-3]}"
        
        try:
            with open(file_path, 'r') as f:
                data = json.load(f)
                
            mean = data.get("mean", {})
            point_estimate = mean.get("point_estimate", 0.0)
            ci = data.get("confidence_interval", {})
            
            results.append({
                "name": bench_name,
                "change": point_estimate,
                "regressed": point_estimate > threshold,
                "lower": ci.get("lower_bound", 0.0),
                "upper": ci.get("upper_bound", 0.0)
            })
                
        except Exception as e:
            print(f"Error reading {file_path}: {e}")

    # Print Summary Table
    print("\n" + "="*80)
    print(f"{'Benchmark':<50} | {'Change':<10} | {'Status':<10}")
    print("-" * 80)
    
    for r in sorted(results, key=lambda x: x['name']):
        change_pct = r['change'] * 100
        status = "❌ FAIL" if r['regressed'] else "✅ PASS"
        if abs(r['change']) < 0.01: status = "  SAME"
        elif r['change'] < -threshold: status = "🚀 IMPROVED"
        
        print(f"{r['name']:<50} | {change_pct:>9.2f}% | {status}")
        
    print("="*80 + "\n")

    regressions = [r for r in results if r['regressed']]
    if regressions:
        print(f"❌ REGRESSIONS DETECTED ({len(regressions)}):")
        for r in regressions:
            print(f"  {r['name']}: {r['change']*100:.2f}% slower")
        return 1
    else:
        print("✅ No significant regressions detected.")
        return 0

if __name__ == "__main__":
    threshold = float(os.environ.get("BENCH_REGRESSION_THRESHOLD", "0.10"))
    target_dir = sys.argv[1] if len(sys.argv) > 1 else "target"
    
    sys.exit(check_regression(target_dir, threshold))