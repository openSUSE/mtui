#!/usr/bin/perl -w
#
# check for valid VENDOR and DISTURL in installed rpm packages
# rommel@suse.de 2011-02-03
#
# tested and supported products;
# - SLES9 SP3 - SP4
# - SLE10 SP1 - SP4
# - SLE11 GA - SP1
# - SLES4VMware
# - openSUSE 11.1 - 11.4
#

use strict;

my $defaultbuilddir = "http://hilbert.nue.suse.com/abuildstat/patchinfo/";
my $query;
my $url;

if ($ARGV[0] =~ /^[0-9a-f]{32}$/i) { $url = $defaultbuilddir . $ARGV[0]; }

my %valid_vendors = (
    "SLE" => [
         "SUSE LINUX Products GmbH, Nuernberg, Germany",
         "SuSE Linux AG, Nuernberg, Germany", # packages shipped before 2004
         "IBM Corp.", # specific to ppc(64) on all SLE products
    ],
    "openSUSE" => [
         "openSUSE",
         "obs://build.suse.de/home:sndirsch:drivers", # 3rd party repackaged drivers (ATI, NVIDIA)
    ],
);

my %valid_disturls = (
    "SLE" => [
         "obs://build.suse.de/SUSE:SLE-11:GA/standard/",
         "obs://build.suse.de/SUSE:SLE-11:GA:Products:Test/standard/",
         "obs://build.suse.de/SUSE:SLE-11:Update:Test/standard/",
         "obs://build.suse.de/SUSE:SLE-11-SP[1-9]+:GA/standard/",
         "obs://build.suse.de/SUSE:SLE-11-SP[1-9]+:GA:Products:Test/standard/",
         "obs://build.suse.de/SUSE:SLE-11-SP[1-9]+:GA:UU-DUD/standard/",
         "obs://build.suse.de/SUSE:SLE-11-SP[1-9]+:Update:Products:Test:Update:Test/standard/",
         "obs://build.suse.de/SUSE:SLE-11-SP[1-9]+:Update:Test/standard/",
         "obs://build.suse.de/SUSE:SLE-11-SP[1-9]+:Update:Test:BlockMigration/standard/",
         "obs://build.suse.de/SUSE:SLE-11-SP[1-9]+:Update:Test:UnBlockMigration/standard/",
         "obs://build.suse.de/SUSE:SLE-11-SP[1-9]+:Update:ATK:[0-9.]+/standard/",
         "obs://build.suse.de/SUSE:SLE-11-SP[1-9]+:Update:ATK:[0-9.]+:Update:Test/standard/",
         "obs://build.suse.de/SUSE:SLE-10-SP[1-9]+:GA/standard/",
         "obs://build.suse.de/SUSE:SLE-10-SP[1-9]+:GA/SLE_[0-9]+_SP[0-9]+_Update/",
         "obs://build.suse.de/SUSE:SLE-10-SP[1-9]+:Update:Test/standard/",
         "srcrep:[0-9a-f]{32,}-",
         # obs://build.suse.de/SUSE:SLE-11:GA/standard/
         # obs://build.suse.de/SUSE:SLE-11:GA:Products:Test/standard/
         # obs://build.suse.de/SUSE:SLE-11:Update:Test/standard/
         # obs://build.suse.de/SUSE:SLE-11-SP1:GA/standard/
         # obs://build.suse.de/SUSE:SLE-11-SP1:GA:Products:Test/standard/
         # obs://build.suse.de/SUSE:SLE-11-SP1:GA:UU-DUD/standard/
         # obs://build.suse.de/SUSE:SLE-11-SP1:Update:Test/standard/
         # obs://build.suse.de/SUSE:SLE-11-SP1:Update:ATK:1.2/standard
         # obs://build.suse.de/SUSE:SLE-11-SP1:Update:ATK:1.2:Update:Test/standard
         # obs://build.suse.de/SUSE:SLE-10-SP4:GA/standard/
         # obs://build.suse.de/SUSE:SLE-10-SP3:GA/SLE_10_SP2_Update/
         # obs://build.suse.de/SUSE:SLE-10-SP3:Update:Test/standard/
         # srcrep:aff578d3a933f0942233ca29b28d5e1c-x11-tools
    ],
    "openSUSE" => [
         "obs://build.opensuse.org/openSUSE:[0-9.]+/standard/",
         "obs://build.opensuse.org/openSUSE:[0-9.]+:Update:Test/standard/",
         "obs://build.opensuse.org/openSUSE:[0-9.]+:NonFree/standard/",
         "obs://build.suse.de/home:sndirsch:drivers/openSUSE_[0-9.]+/",
         "obs://build.suse.de/SUSE:openSUSE:11.1:Update:Test/standard/",
         "obs://build.opensuse.org/openSUSE:Maintenance:",
         "srcrep:[0-9a-f]{32,}-",
         # obs://build.opensuse.org/openSUSE:11.2/standard/
         # obs://build.opensuse.org/openSUSE:11.2:Update:Test/standard/
         # obs://build.opensuse.org/openSUSE:11.3:NonFree/standard/
         # obs://build.suse.de/home:sndirsch:drivers/openSUSE_11.3/
         # obs://build.suse.de/SUSE:openSUSE:11.1:Update:Test/standard/
         # obs://build.opensuse.org/openSUSE:Maintenance:25/openSUSE_12.1_standard/f967c9dd1d403fd0275a13b87a2f5d56-bind.openSUSE_12.1
         # srcrep:1e79d7e8a1e89516f0d4ce57ecf3d01a-zlib
    ],
);

my @sle_checks = (
                   "test -d /var/adm/YaST/ProdDB && grep \"SUSE SLES Version 9\" /var/adm/YaST/ProdDB/prod_\*",
                   "test -x /usr/lib\*/zmd/query-pool && /usr/lib\*/zmd/query-pool products \@system | grep SUSE_SLE",
                   "test -d /etc/products.d && grep \"<distribution>SUSE_SLE</distribution>\" /etc/products.d/\*",
);

my @opensuse_checks = (
                   "test -d /etc/products.d && grep \"<distribution>openSUSE</distribution>\" /etc/products.d/\*",
);

my $productclass = undef;

foreach my $check (@sle_checks) {
    if ( `$check` =~ /\S+/) {
        $productclass = "SLE";
        last;
    }
}

if (not defined $productclass) {
    foreach my $check (@opensuse_checks) {
        if ( `$check` =~ /\S+/) {
            $productclass = "openSUSE";
            last;
        }
    }
}

if (not defined $productclass) {
   print STDERR "ERROR: detected none of openSUSE and SLE products being installed ... aborting.\n";
   exit 1;
}

print "INFO: detected product class: $productclass\n";

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

open (FH, "-|", "rpm -q --qf \"\%{NAME} %{DISTURL} %{VENDOR}\n\" $query | sort -t - -k1,5") or die;
while (<FH>) {
    next if /is not installed/; 
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

