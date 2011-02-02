#!/usr/bin/perl -w
# need to test on
# - 10 SP2: dax
# - openSUSE 11.3: boxer
# - ia64, s390x and ppc (any sle10 and sle11 and sle11sp1): 3 tests on refhosts
# - SLES4VMware

use strict;

my %valid_vendors = (
    "SLE" => [
         "SUSE LINUX Products GmbH, Nuernberg, Germany"
    ],
    "openSUSE" => [
          "openSUSE",
    ],
);

my %valid_disturls = (
    "SLE" => [
         "obs://build.suse.de/SUSE:SLE-11:GA/standard/",
         "obs://build.suse.de/SUSE:SLE-11:GA:Products:Test/standard/",
         "obs://build.suse.de/SUSE:SLE-11:Update:Test/standard/",
         "obs://build.suse.de/SUSE:SLE-11-SP[1-9]+:GA/standard/",
         "obs://build.suse.de/SUSE:SLE-11-SP[1-9]+:GA:UU-DUD/standard/",
         "obs://build.suse.de/SUSE:SLE-11-SP[1-9]+:Update:Test/standard/",
         "obs://build.suse.de/SUSE:SLE-10-SP[1-9]+:GA/SLE_[0-9]+_SP[0-9]+_Update/",
         "obs://build.suse.de/SUSE:SLE-10-SP[1-9]+:Update:Test/standard/",
         "srcrep:[0-9a-f]{32,}-",
         # obs://build.suse.de/SUSE:SLE-11:GA/standard/
         # obs://build.suse.de/SUSE:SLE-11:GA:Products:Test/standard/
         # obs://build.suse.de/SUSE:SLE-11:Update:Test/standard/
         # obs://build.suse.de/SUSE:SLE-11-SP1:GA/standard/
         # obs://build.suse.de/SUSE:SLE-11-SP1:GA:UU-DUD/standard/
         # obs://build.suse.de/SUSE:SLE-11-SP1:Update:Test/standard/
         # obs://build.suse.de/SUSE:SLE-10-SP3:GA/SLE_10_SP2_Update/
         # obs://build.suse.de/SUSE:SLE-10-SP3:Update:Test/standard/
         # srcrep:aff578d3a933f0942233ca29b28d5e1c-x11-tools
    ],
    "openSUSE" => [
         "obs://build.opensuse.org/openSUSE:[0-9.]+/standard/",
         "obs://build.opensuse.org/openSUSE:[0-9.]+:Update:Test/standard/",
         # obs://build.opensuse.org/openSUSE:11.2/standard/
         # obs://build.opensuse.org/openSUSE:11.2:Update:Test/standard/
    ],
);

my $is_sle = 0;
my $is_opensuse = 0;
my $productclass = undef;

my @sle_checks = (
                   "test -x /usr/lib\*/zmd/query-pool && /usr/lib\*/zmd/query-pool products \@system | grep SUSE_SLE",
                   "test -x /usr/bin/zypper && /usr/bin/zypper search -t product --installed-only | grep SUSE_SLE",
);

my @opensuse_checks = (
                       "test -x /usr/bin/zypper && /usr/bin/zypper search -t product --installed-only | grep openSUSE"
);

foreach my $check (@sle_checks) {
    my $result = `$check`;
    if ($result =~ /\S+/) {
        $productclass = "SLE";
        $is_sle = 1;
        last;
    }
}

if ($is_sle == 0) {
    foreach my $check (@opensuse_checks) {
        my $result = `$check`;
        if ($result =~ /\S+/) {
            $productclass = "openSUSE";
            $is_opensuse = 1;
            last;
        }
    }
}

if ($is_opensuse + $is_sle == 0) {
   print STDERR "ERROR: detected none of openSUSE and SLE products being installed ... aborting.\n";
   exit 1;
}

print "INFO: detected product class: $productclass\n";

open (FH, "-|", "rpm -qa --qf \"\%{NAME} %{DISTURL} %{VENDOR}\n\"") or die;
while (<FH>) {
    my ($package, $disturl, @remainder) = split (/\s+/);
    my $vendor = join (" ", @remainder);

    next if ($package =~ /^gpg-pubkey/);
    next if ($disturl =~ /obs:\/\/build.suse.de\/QA:/);

    my $matched_vendor = 0;
    foreach my $possible_match (@{$valid_vendors{$productclass}}) {
        if ($vendor =~ /$possible_match/) {
            $matched_vendor = 1;
            last;
        }
    }
    if ($matched_vendor == 0) {
        print STDERR "ERROR: package $package has an alien vendor string: \"$vendor\"\n";
    }

    my $matched_disturl = 0;
    foreach my $possible_match (@{$valid_disturls{$productclass}}) {
        if ($disturl =~ /$possible_match/) {
            $matched_disturl = 1;
            last;
        }
    }
    if ($matched_disturl == 0) {
        print STDERR "ERROR: package $package has an alien disturl string: \"$disturl\"\n";
    }

}
close (FH);

