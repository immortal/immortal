BEGIN {
    FS = "\t"
    metric_order[1] = "fork-spawn-wait"
    metric_order[2] = "fork-spawn-signal-wait"
    valid[metric_order[1]] = 1
    valid[metric_order[2]] = 1
}

{
    metric = $1
    if (!(metric in valid)) {
        fail(FILENAME ": unknown benchmark metric: " metric)
        next
    }
    if ($2 !~ /^[0-9]+ ns\/op$/) {
        fail(FILENAME ": malformed benchmark measurement: " $2)
        next
    }
    key = FILENAME SUBSEP metric
    if (key in seen) {
        fail(FILENAME ": duplicate benchmark metric: " metric)
        next
    }
    split($2, measurement, " ")
    if (length(measurement[1]) > 15 || measurement[1] + 0 < 1) {
        fail(FILENAME ": benchmark measurement is outside the exact numeric range")
        next
    }
    value = measurement[1] + 0
    seen[key] = 1
    files[FILENAME] = 1
    count[metric]++
    values[metric, count[metric]] = value
}

END {
    for (file in files) {
        for (order_index = 1; order_index <= 2; order_index++) {
            metric = metric_order[order_index]
            if (!((file SUBSEP metric) in seen)) {
                fail(file ": missing benchmark metric: " metric)
            }
        }
    }
    if (failed) {
        exit 65
    }
    if (count[metric_order[1]] != count[metric_order[2]]) {
        fail("benchmark metrics have different run counts")
        exit 65
    }

    print "platform\tmetric\truns\tmedian_ns\tmax_ns\tp95_ns\tsuggested_ceiling_ns"
    for (order_index = 1; order_index <= 2; order_index++) {
        metric = metric_order[order_index]
        runs = count[metric]
        sort_values(metric, runs)
        median = median_value(metric, runs)
        maximum = values[metric, runs]
        p95_index = int((95 * runs + 99) / 100)
        p95 = values[metric, p95_index]
        ceiling = int((125 * maximum + 99) / 100)
        print platform "\t" metric "\t" runs "\t" median "\t" maximum "\t" p95 "\t" ceiling
    }
}

function fail(message) {
    print message > "/dev/stderr"
    failed = 1
}

function sort_values(metric, runs,    left, right, candidate) {
    for (right = 2; right <= runs; right++) {
        candidate = values[metric, right]
        left = right - 1
        while (left >= 1 && values[metric, left] > candidate) {
            values[metric, left + 1] = values[metric, left]
            left--
        }
        values[metric, left + 1] = candidate
    }
}

function median_value(metric, runs,    upper) {
    upper = int(runs / 2) + 1
    if (runs % 2 == 1) {
        return values[metric, upper]
    }
    return int((values[metric, upper - 1] + values[metric, upper] + 1) / 2)
}
