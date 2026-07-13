BEGIN {
    FS = "\t"
    expected_header = "schema_version\tcommit\tplatform\tkernel\tcpu\ttoolchain\tsupervisor\tsupervisor_version\tscenario\tsample\tmetric\tvalue\tunit\toutcome\tcleanup"
}

NR == 1 {
    if ($0 != expected_header) {
        report("invalid validation evidence header")
    }
    next
}

{
    if (NF != 15) {
        report("validation evidence row must contain exactly 15 fields")
        next
    }
    if ($1 != "1") {
        report("unsupported validation evidence schema")
    }
    if ($2 == "" || $3 == "" || $4 == "" || $5 == "" || $6 == "" ||
        $7 == "" || $8 == "" || $9 == "" || $11 == "" || $13 == "") {
        report("validation evidence contains an empty required field")
    }
    if ($10 !~ /^[1-9][0-9]*$/) {
        report("validation evidence sample must be a positive integer")
    }
    if ($12 !~ /^-?[0-9]+([.][0-9]+)?$/) {
        report("validation evidence value must be a base-ten number")
    }
    if ($14 != "pass" && $14 != "fail") {
        report("validation evidence outcome must be pass or fail")
    }
    if ($15 == "") {
        report("validation evidence cleanup result is empty")
    }
}

END {
    if (NR < 2) {
        report("validation evidence contains no result rows")
    }
    exit failed
}

function report(message) {
    print FILENAME ":" NR ": " message > "/dev/stderr"
    failed = 1
}
