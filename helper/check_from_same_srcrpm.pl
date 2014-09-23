#!/usr/bin/perl -w

use strict;

my %checksums;

sub package_srcrpm_checksum {
   my $string = shift;
   # kernel-default obs://build.suse.de/SUSE:SLE-12:GA/standard/2f9d4c1ae431fa208495e24d3079d024-kernel-default

   my ($package, $disturl) = split (/\s+/, $string);

   my ($srcrpm, $checksum);

   if ($disturl =~ /(:|\/)([0-9a-f]{32,})-([^\/]*)/) {
       ($checksum, $srcrpm) = ($2, $3);
   }
   else {
       if (not $package =~ /^gpg-pubkey/) {
           # gpg-pubkeys are pseudo rpm packages which don't have a DISTURL et al
           print STDERR "WARNING: skipping \"$package\" since unable to decompose DISTURL \"$disturl\"\n";
       }
       return (undef,undef,undef);
   }

   return ($package, $srcrpm, $checksum);
}

#
# build a whitelist for all packages that match on any of the multiversion attributes from /etc/zypp/zypp.conf
#

my %multiversioned;
my @multiversionfilters;

my $found = open (FH, "< /etc/zypp/zypp.conf");
if ($found > 0) {
   while (<FH>) {
       # multiversion = provides:multiversion(kernel), foobar
       if (/^\s*multiversion\s*=\s*(\S+)/) {
          @multiversionfilters = split (/,/, $1);
       }
   }
}
close (FH);

foreach my $filter (@multiversionfilters) {
   my $querycommand;
   if ($filter=~ /provides:(\S+)/) {
      $querycommand = "-q --whatprovides \"$1\"";
   }
   else {
      $querycommand = "-q $1";
   }
   # print "DEBUG: $querycommand\n";

   open (FH, "/bin/rpm $querycommand --queryformat \"%{NAME} %{DISTURL}\n\" | sort -t ' ' -k1,1 |");

   while (<FH>) {
      my ($package, $srcrpm, $checksum) = package_srcrpm_checksum ($_);
      next if not defined $checksum;

      $multiversioned{$package}{$checksum} = "yes";
      print "INFO: is_multiversioned: $package from src rev $checksum\n";
   }
   close (FH);
}

#
# check all installed rpms for the revisions of their src rpms
#

open (FH, "/bin/rpm -qa --queryformat \"%{NAME} %{DISTURL}\n\" | sort -t ' ' -k1,1 |");

while (<FH>) {
    my ($package, $srcrpm, $checksum) = package_srcrpm_checksum ($_);
    next if not defined $checksum;

    # do not track multiversioned packages

    if (not defined($multiversioned{$package}{$checksum})) {
       push (@{$checksums{$srcrpm}{$checksum}}, $package);
       # print "DEBUG: tracking: $package, $srcrpm, $checksum\n";
    }
}
close (FH);

foreach my $srcrpm (sort keys %checksums) {
    if (keys %{$checksums{$srcrpm}} > 1) {
        print STDERR "ERROR: different src checksums found for packages build from $srcrpm:\n";
        foreach my $checksum (sort keys %{$checksums{$srcrpm}}) {
            print STDERR "\t$checksum: " . join (",", sort @{$checksums{$srcrpm}{$checksum}}) . "\n";
        }
    }
}

exit 0;
