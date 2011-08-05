#!/usr/bin/perl -w
# vim: sw=4 et
# idea and prototype by Dirk Mueller <dmueller@suse.de> in 2010
# finished by Heiko Rommel <rommel@suse.de> in 2011

use strict;
use Getopt::Long;
use File::Temp qw(tempfile);

my $help;
my $installed;
my $build;
my $filter;
my $quiet;
my $mismatches = 0;
my $consideredpackages = 0;
my $skippedpackages = 0;
my $ibs = "https://api.suse.de/public/";
my $obs = "https://api.opensuse.org/public/";
my $defaultbuilddir = "http://hilbert.suse.de/abuildstat/patchinfo/";
my %disturl_mapper;
my %disturl_packages;
my %buildsrcnames;
my $dir;

my $usagemsg = "
usage:\t$0 [--help] [--quiet] 
           (--installed [--filter <url to build dir>] | --build <url to build dir>)

This script operates in two modes:

When using --installed then all installed packages that have been built in
related update projects (e.g. have been previously updated on the system) are
checked if source in the update project exists that superseeds the installed
version. Additionaly, if you specify the option --filter then the list of
installed packages is first filtered if they have been built from the same src
rpm(s) as the the packages in the build dir. This is mostly usefull to speed
up/limit verification to a set of packages. 

The argument to --filter can be either a web url 
(like http://hilbert.suse.de/abuildstat/patchinfo/ad8b1800d6dc90608d0c5a7103bc1839/)
or a local path (like /mounts/work/built/patchinfo/ad8b1800d6dc90608d0c5a7103bc1839).

Note: packages that have NOT been updated are skipped (not validated) since
from the DISTURL of the installed packages we can not guess the correct update
project (especially on SLE11 with overlayed update repos).

When using --build then all packages located in the build dir are checked if
source in the update project exists that superseeds the built packages. 
This is totally independent from the installed packages.

Note: at this point we assume that no packages exist in the build dir that have been
built from different versions of the same src rpm (this should only happen if
a build service engineer manually tampered with the build dir ;).

Use the option --quiet to not output the diff of the changelogs of the
mismatching source revisions.
";

GetOptions(
     "h|help" => \$help,
     "i|installed" => \$installed,
     "b|build=s" => \$build,
     "f|filter=s" => \$filter,
     "q|quiet" => \$quiet,
)  or die "$usagemsg";

if (defined $help) {
   print $usagemsg;
   exit 0;
}

# work around the requirement of having arguments (sth/ mtui.py currently can not provide)
if (not defined $build and not defined $installed) { 
    my $firstarg = shift;
    if (defined $firstarg) { 
        $installed = 'true'; 
        $filter = $defaultbuilddir . $firstarg;
        print "INFO: assuming options \"--installed --filter $filter\"\n";
    }
}

if ((not defined $installed and not defined $build) or
    (defined $installed and defined $build)) {
   print STDERR "ERROR: bad set of command line arguments\n$usagemsg";
   exit 1;
}

sub geturlsofsrcrpms {
    my $url = shift or die;
    my @srcrpms;

    if ($url =~ /^http/) {
        open (IN, "-|", "w3m -dump $url");
        while (<IN>) {
            if (/\[DIR\]\s+(\S+)\//i) {
                my $subdir = $1;
                open (INS, "-|", "w3m -dump $url/$subdir");
                while (<INS>) {
                    if (/\s+(\S+\.(no)?src\.rpm)\s+/i) {
                        my $srcrpm = $1;
                        push (@srcrpms, "$url/$subdir/$srcrpm");
                    }
                }
                close (INS);
            }   
        }   
        close (IN);
    }
    else {
        die "ERROR: unable to access build directory $dir\n" if (not -d $url or not -r $url);
        @srcrpms = split (/\n/, `find $url -iname '*\.src\.rpm' -or -iname '*\.nosrc\.rpm'`);
    }

    return @srcrpms;
}

$dir = (defined $build) ? $build : (defined $filter) ? $filter : undef;

if (defined $dir) {
    my @srcrpms = geturlsofsrcrpms($dir);
    foreach my $srcrpm (@srcrpms) {                                                                       
        open (IN, "-|", "rpm -qp --qf \"%{NAME} %{DISTURL}\n\" $srcrpm | sort -t - -k1,5") or die;
        while (<IN>) {
            my ($srcname, $disturl) = split;
            if (defined $build) { 
                $disturl_mapper{$disturl} = $srcname; 
                push (@{$disturl_packages{$disturl}}, $srcname); 
                $consideredpackages++;
            }
            elsif (defined $filter) { 
                $buildsrcnames{$srcname}++; 
            }
            # print "DEBUG: src rpm $srcname references $disturl\n";
        }
        close (IN);
    }

    if (open(P, "$dir/patchinfo")) {
       print grep { /SUBSWAMPID:/ } <P>;
       close(P);
    }
}

if (defined $installed) {
    open (IN, "-|", "rpm -qa --qf \"%{NAME} %{DISTURL}\n\" | sort -t - -k1,5 ") or die;
    while (<IN>) {
        my ($package, $disturl) = split;
        next if ($package =~ /^gpg-pubkey/);

        my ($srcname) = ($disturl =~ m/\/[0-9a-f]{32,}-([^\/]*)/);
        if (not defined $srcname) {
            print "INFO: unable to get a DISTURL from installed package $package ... skipping\n";
            $skippedpackages++;
            next;
        }
        else { 
            $disturl_mapper{$disturl} = $srcname;
            push (@{$disturl_packages{$disturl}}, $package); 
            if (not defined $filter or defined $buildsrcnames{$srcname}) {
                $consideredpackages++;
            }
        }
        # print "DEBUG: installed $package references $disturl from src rpm $srcname\n";
    }
    close (IN);
}

if ($consideredpackages == 0 and defined $installed) {
    print STDERR "ERROR: from the installed packages not a single could be verified (no updates applied?)!\n";
    exit 1;
}

my $affectedmessage = (defined $installed) ? "affecting installed package(s)" : "affecting built src rpm(s)";

while (my ($disturl, $name) = each %disturl_mapper) {
    # skip if DISTURL points to a GM project like
    # SLE 11 SP0:
    # obs://build.suse.de/SUSE:SLE-11:GA/standard
    # SLE 11 SP1:
    # obs://build.suse.de/SUSE:SLE-11-SP1:GA/standard/
    # SLE 10 SP3:
    # obs://build.suse.de/SUSE:SLE-10-SP3:GA/SLE_10_SP2_Update
    # openSUSE 11.2:
    # obs://build.opensuse.org/openSUSE:11.2/standard/
    if (
        $disturl =~ m,/build.suse.de/SUSE:SLE-.*:GA/standard/, or
        $disturl =~ m,/build.opensuse.org/openSUSE:[0-9.]+/standard/,
       ) {
        $skippedpackages += @{$disturl_packages{$disturl}};
        next;
    }

    # in case we validate against a specifc maintenance update skip if the src
    # name is not among the src names of the maintenance update
    if (defined $filter and not defined $buildsrcnames{$name}) {
        next;
    }

    # skip in case the the DISTURL does not point to either the internal or external
    # build service for SUSE, openSUSE or QA
    if (
        not $disturl =~ m,obs://build.suse.de/(SUSE|QA), and
        not $disturl =~ m,obs://build.opensuse.org/openSUSE,
        ) {
        $skippedpackages += @{$disturl_packages{$disturl}};
        next;
    }

    # at this point DISTURL should point to a project where updates are kept
    # SLE 11 SP0:
    # obs://build.suse.de/SUSE:SLE-11:Update:Test/standard/
    # obs://build.suse.de/QA:SLE11/...
    # SLE 11 SP1:
    # obs://build.suse.de/SUSE:SLE-11-SP1:Update:Test/standard/
    # obs://build.suse.de/QA:SLE11SP1/...
    # SLE 10 SP3:
    # obs://build.suse.de/SUSE:SLE-10-SP3:Update:Test/standard
    # obs://build.suse.de/QA:SLE10SP3/...
    # openSUSE 11.2:
    # obs://build.opensuse.org/openSUSE:11.2:Update:Test/standard/
    # obs://build.opensuse.org/openSUSE:11.2:NonFree/standard/
    # obs://build.opensuse.org/openSUSE:11.2:Contrib/standard/
    # obs://build.suse.de/QA:Head/openSUSE_11.2

    my ($prj, $md5pkg) = (split "/", $disturl)[3, 5];
    my ($src_revision) = (split "-", $md5pkg)[0];
    my $publicapi = ($disturl =~ /build.suse.de/) ? $ibs : ($disturl =~ /build.opensuse.org/) ? $obs : undef;
    # print "DEBUG: $disturl -> API $publicapi\n";

    open(BS, "-|", "curl", "-s", "-k", "$publicapi/source/$prj/$name?expand") or die;
    while(<BS>) {
        chomp;
        my $r = $_;

        if ($r =~ m/<directory.*/) {
            my ($current_rev) = ($r =~ m/<directory.*srcmd5=\"([^\"]+)\"/);
            if ($disturl !~ /$current_rev/) {
                $mismatches += @{$disturl_packages{$disturl}};
                print STDERR "ERROR: some packages from source $name seem to be outdated\n" .
                             "       $affectedmessage " . join (" ", @{$disturl_packages{$disturl}}) . "\n" .
                             "       have src revision $src_revision but expected $current_rev\n" .
                             "       DISTURL=$disturl\n";

                if (not defined $quiet) {
                    print        "       changelog of each contained spec file:\n";
                    while(<BS>) {
                        if (m/<entry name=\"([^\"]+\.changes)\"/) {
                            my ($changes, $spec) = ($1, $1);
                            $spec =~ s/.changes$//;
                            print "\n       $spec:\n";
                            print "       " . "-" x (length($spec) + 1) . "\n";

                            my (undef, $orig_file) = tempfile();
                            my (undef, $expect_file) = tempfile();
                            my (undef, $diff_file) = tempfile();

                            `curl -s -k $publicapi/source/$prj/$name/$changes?rev=$src_revision > $orig_file`;
                            `curl -s -k $publicapi/source/$prj/$name/$changes?rev=$current_rev > $expect_file`;
                            `diff --label $changes -u $orig_file --label $changes $expect_file > $diff_file`;
                            if ( -s $diff_file ) { print `cat $diff_file`; }
                            else { print "       (empty diff)\n\n"; }

                            unlink($orig_file);
                            unlink($expect_file);
                            unlink($diff_file);
                        }
                    }
                    print "\n";
                }
            }
            last;
        }
    }
    close(BS);
}

print "INFO: $mismatches mismatches among the $consideredpackages considered packages could be detected (" . 
      int($mismatches/$consideredpackages*100) . "%)\n";

if (defined $installed) { 
    print "INFO: the DISTURL of $skippedpackages installed packages does not point to a known update project (never updated?)\n";      
}

exit 0;
