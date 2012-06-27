#!/usr/bin/perl -w

use strict;

my $query;
my %checksums;
my $defaultbuilddir = "http://hilbert.nue.suse.com/abuildstat/patchinfo/";
my $url;

if ($ARGV[0] =~ /^[0-9a-f]{32}$/i) { $url = $defaultbuilddir . $ARGV[0]; }

sub getpackagelist {
    my $url = shift or return;
    my %packages;

    open (IN, "-|", "w3m -dump $url");
    while (<IN>) {
        if (/\[DIR\]\s+(\S+)\//i) {
            my $subdir = $1;
            open (INS, "-|", "w3m -dump $url/$subdir");
            while (<INS>) {
                next if /\.delta\./;
                if (m/] (.+)-([^-]+)-([^-]+)\.(\w+)\.rpm/i) {
                    $packages{$1} = "";
                }
            }
           close (INS);
        }
    }
    close (IN);

    return %packages;
}

my @packages = getpackagelist($url);

# if no packages were returned, check all installed packages
if (@packages) {
    $query = "@packages";
} else {
    $query = "-a";
}

open (FH, "/bin/rpm -q --queryformat \"%{NAME} %{DISTURL}\n\" $query | sort -t - -k1,5 |");
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
