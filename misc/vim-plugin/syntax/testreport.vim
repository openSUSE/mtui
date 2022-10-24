" Vim syntax file
" Language: QAM Testreport
" Maintainer: Jan Baier
" Latest Revision: 24 Oct 2022

if exists("b:current_syntax")
    finish
endif

" Keywords
syn keyword positiveKeyword PASSED SUCCEEDED YES FIXED
syn keyword negativeKeyword FAILED NO NOT_FIXED HYPOTHETICAL NOT_REPRODUCIBLE NO_ENVIRONMENT TOO_COMPLEX SKIPPED OTHER

" Matches
syn match Comment 'For more details see "regression testing" below.'
syn match Identifier /\s*=>/
syn match Identifier /^[A-Z][A-Za-z_ ]\+:/
syn match Label /^#\+\n.*\n#\+$/
syn match Label /^.\+:\?\n-\+$/
syn match Label /^\(.* \)\?SUMMARY:\?\(\n=\+\)\?/
syn match Label /^METADATA:\?\(\n=\+\)\?/
syn match Macro /\(new \)\?bugs.*:$/
syn match Macro /^\(before\|after\|scripts\):/
syn match QAMComment /comment:.*/
syn match Error "(put your details here)"
syn match Error "Suggested Test Plan Reviewers: .*"
syn match Error /Example.*:/
syn match Error /^SUMMARY:\s*PASSED\/FAILED/
syn match Error /^REPRODUCER_PRESENT:\s*YES\/NO/
syn match Error /^STATUS:\s*[^/]\+\(\/[^/]\+\)\+/
syn match Error /^TEST_SUITE_PRESENT:\s*YES\/NO/
syn match Error /^NEW_VERSION_OR_NEW_PACKAGE:\s*YES\/NO/
syn match Error /^ALL_TRACKED_ISSUES_DOCUMENTED:\s*YES\/NO/
syn match Error /^HAS_UNTRACKED_CHANGES:\s*YES\/NO/

" Region
syn region Comment start="put here the output of the following commands:" end="## export MTUI:.*"
syn region Comment start="List of testcases in Testopia:" end="https://bugzilla.*"
syn region Comment start="Put here the assessment" end="report directory."
syn region Comment start="In case of FAILED" end="etc)."

" Highlights
hi negativeKeyword ctermfg=darkred
hi positiveKeyword ctermfg=darkgreen
hi QAMComment ctermfg=white
