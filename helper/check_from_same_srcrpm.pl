#!/usr/bin/perl -w

use strict;

my %checksums;

open (FH, "/bin/rpm -qa --queryformat \"%{NAME} %{DISTURL}\n\" | sort -t ' ' -k1,1 |");
while (<FH>) {
    next if /is not installed/;
    my ($package, $disturl) = split (/\s+/);
    my ($srcrpm, $checksum);

    if ($disturl =~ /(:|\/)([0-9a-f]{32,})-([^\/]*)/) {
        ($checksum, $srcrpm) = ($2, $3);
    }
    else {
        if (not $package =~ /^gpg-pubkey/) {
            print STDERR "WARNING: unable to decompose DISTURL \"$disturl\" from package \"$package\" ... skipped\n";
        }
        next;
    }

    # print "DEBUG: $package, $disturl, $srcrpm, $checksum\n";
    push (@{$checksums{$srcrpm}{$checksum}}, $package);
}
close (FH);

foreach my $srcrpm (sort keys %checksums) {
    if (keys %{$checksums{$srcrpm}} > 1) {
        print STDERR "ERROR: different src checksums found for packages build from $srcrpm:\n";
        foreach my $checksum (sort keys %{$checksums{$srcrpm}}) {
            print STDERR "\t$checksum: " . join (",", @{$checksums{$srcrpm}{$checksum}}) . "\n";
        }
    }
}

exit 0;
