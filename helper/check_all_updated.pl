#!/usr/bin/perl -w
# vim: sw=4 et
# idea and prototype by Dirk Mueller <dmueller@suse.de> in 2010 (--build case)
# continued by Heiko Rommel <rommel@suse.de> in 2011 (--installed case)

use strict;
use Getopt::Long;
use File::Temp qw(tempfile);

my $usagemsg = "
usage:\t$0 [--help] [--verbose] [--debug]
           (--installed [-r <url-to-remote-repo> -p <path-to-filelist>] |
            --build -r <url-to-remote-repo> -p <path-to-filelist>) id

id is either \$md5sum or (openSUSE|SUSE):Maintenance:\$issue:\$request

This script operates in two modes:

When using --installed then all installed packages that have been built in update projects (e.g. have been previously updated on the system) are checked if source in the update project exists that superseeds the installed version. 
Additionaly, if you specify the options -r and -p then the list of installed packages is first filtered if they have been built from the same src rpm(s) as the the packages at the given location. This is mostly usefull to speed up/limit verification to a set of packages. 

When using --build and mandatory -r and -p then all packages located in the remote dir are checked if source in the update project exists that superseeds the built packages.  This is totally independent from the installed packages and used to verify if this is an outdated maintenance submission.

The repo url given by -r and the package list given by -p are used to compose the full path to the build packages. Typicall patterns:

   SLE >= 12
   r = http://download.suse.de/ibs/SUSE:/Maintenance:/32/
   p contains items like: SUSE_Updates_SLE-SERVER_12_aarch64/aarch64/libvirt-2.2.5-9.1.aarch64.rpm

   SLE < 12
   r = http://hilbert.nue.suse.com/abuildstat/patchinfo/ad8b1800d6dc90608d0c5a7103bc1839/
   p contains items like: sle11-i586/nss_ldap-x86-262-11.32.39.1.ia64.rpm

   openSUSE >= 12.*
   r = http://download.opensuse.org/repositories/openSUSE:/Maintenance:/2971/
   p contains items like: openSUSE_13.1_Update/x86_64/accountsservice-0.6.35-2.16.1.x86_64.rpm  

   PTFs
   r = http://euklid.suse.de/mirror/SuSE/support.suse.de/
   p contains items like: x86_64/update/SUSE-SLES/10/PTF/b27a428a0750dc195e58933ba4411674/20110321/???

Note: packages that have NOT been updated are skipped (not validated) since from the DISTURL of the installed packages we can not guess the correct update project (especially on SLE11 with overlayed update repos)

Note: the assumption is that no packages exist in <url to build dir> that have been built from different versions of the same src rpm (this should only happen if a build service engineer manually tampered with the build dir ;)

Use the option --verbose to output the diff of the changelogs of the mismatching source revisions.
Use the option --debug to trigger debug out.
";

my $installed;
my $build;
my $verbose;
my $debug;
my $repo;
my $plist;
my $help;
my $id;

GetOptions(
     "i|installed" => \$installed,
     "b|build" => \$build,
     "v|verbose" => \$verbose,
     "d|debug" => \$debug,
     "r=s" => \$repo,
     "p=s" => \$plist,
     "h|help" => \$help,
     ) or die "$usagemsg";

$id=shift;

if (defined $help) {
    print $usagemsg;
    exit 0;
}

if (defined $installed and defined $build) {
   print STDERR "ERROR: bad set of command line arguments\n$usagemsg";
   exit 1;
}

if (defined $build and (not defined $repo or not defined $plist)) {
   print STDERR "ERROR: -p and -r are mandatory for --build\n$usagemsg";
   exit 1;
}

# currently, mtui.py calls the script with a fixed set of options,
# neither --installed nore --build is part of that set
# thus, we assume the --installed case

if (not defined $build and not defined $installed) { 
   $installed = 'true'; 
   print "INFO: assuming --installed\n";
}

my $ibs = "https://api.suse.de/public/";
my $obs = "https://api.opensuse.org/public/";

my %disturl_mapper;
my %disturl_packages;
my %buildsrcnames;

sub mydebug {
    my $string = shift;
    return unless defined $debug;
    print "DEBUG: $string";
}

sub geturlsofsrcrpms {

     my $repo = shift or die;
     my $file = shift or die;
     my %srcrpms;

     local *FH;
     open (FH, "< $file") or die "ERROR: can't open file $file: $!";

     while (<FH>) {
         if (/(^.*\/(.*\.(src|nosrc)\.rpm))/) {
            $srcrpms{$2} = $repo . "$1";
            mydebug("adding srcrpm $1 for key $2\n");
         }
     }

     close (FH);
     return values %srcrpms;
}

my $mismatches = 0;
my $consideredpackages = 0;
my $skippedpackages = 0;
my $installedpackages = 0;

if (defined $repo and defined $plist) {
    my @srcrpms = geturlsofsrcrpms($repo, $plist);
    foreach my $srcrpm (@srcrpms) {                                                                       
        open (IN, "-|", "rpm -qp --qf \"%{NAME} %{DISTURL}\n\" $srcrpm | sort -t - -k1,5") or die;
        while (<IN>) {
            my ($srcname, $disturl) = split;
            if (defined $build) { 
                $disturl_mapper{$disturl} = $srcname; 
                push (@{$disturl_packages{$disturl}}, $srcname); 
                $consideredpackages++;
            }
            else { 
                $buildsrcnames{$srcname}++; 
                mydebug("\$buildsrcnames{'$srcname'} = " . $buildsrcnames{$srcname} . "\n");
            }
            mydebug("src rpm $srcname references $disturl\n");
        }
        close (IN);
    }
}

if (defined $installed) {
    open (IN, "-|", "rpm -qa --qf \"%{NAME} %{DISTURL}\n\" | sort -t - -k1,5 ") or die;
    while (<IN>) {
        my ($package, $disturl) = split;
        next if ($package =~ /^gpg-pubkey/);
        $installedpackages++;    

        my ($srcname) = ($disturl =~ m/\/[0-9a-f]{32,}-([^\/\.]*)/);
        if (not defined $srcname) {
            print "INFO: unable to get a DISTURL from installed package $package ... skipping\n";
            $skippedpackages++;
            next;
        }
        else { 
            $disturl_mapper{$disturl} = $srcname;
            push (@{$disturl_packages{$disturl}}, $package); 
            mydebug("case 'defined installed': srcname = $srcname\n");
            if (not defined $plist or defined $buildsrcnames{$srcname}) {
                $consideredpackages++;
            }
        }
        mydebug("installed $package references $disturl from src rpm $srcname\n");
    }
    close (IN);

    if ($consideredpackages == 0) {
       print STDERR "ERROR: from the installed packages not a single could be verified (no updates applied?)!\n";
       exit 1;
    }
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
    if (defined $plist and not defined $buildsrcnames{$name}) {
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
    mydebug("$disturl -> API $publicapi\n");

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

                if ($verbose) {
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

my $rate;

$rate = ($consideredpackages != 0) ? int($mismatches/$consideredpackages*100) : "(nan)";
print "INFO: $mismatches mismatches among the $consideredpackages considered packages could be detected ($rate%)\n";

$rate = ($installedpackages != 0) ? int($skippedpackages/$installedpackages*100) : "(nan)";
if (defined $installed) { 
    print "INFO: the DISTURL of $skippedpackages out of $installedpackages installed packages does not point to a known update project (" .
    "$rate% never updated)\n";
}

exit 0;
