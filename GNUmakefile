TMPDIR=/tmp/mtui-unittests
TMPENV=TMPDIR=$(TMPDIR)
NOSE=nosetests tests
COVERAGE=--with-coverage --cover-package=mtui

.PHONY: help
helps+= help "what you are reading"
help:

	@printf "Targets:\n\n"
	@printf "%-16s - %s\n" $(helps)


.PHONY: check
helps += check "run unit tests"
check: tmpdir

	$(TMPENV) $(NOSE)

.PHONY: checkcover
helps += checkcover "run unit tests with coverage"
checkcover: clean .coverage

.coverage: tmpdir

	$(TMPENV) $(NOSE) $(COVERAGE)

.PHONY: annotate
helps += annotate "annotate source code with execution information"
annotate: .coverage

	find ./mtui -name '*.py' -exec coverage annotate {} \;
# using coverage annotate -d generates shitty filenames

.PHONY: tmpdir
tmpdir: clean_unittests_temp

	mkdir $(TMPDIR)

.PHONY: clean
helps += clean "clean all the artefacts!"
clean: clean_coverage clean_unittests_temp

.PHONY: clean_coverage
helps += clean_coverage "clean coverage files"
clean_coverage:

	$(RM) .coverage
	find -name '*.py,cover' -delete

.PHONY: clean_unittest_temp
helps += clean_unittests_temp "clean temporary files created by unittests"
clean_unittests_temp:

	if test -d $(TMPDIR); then \
		chmod u+rwX -R $(TMPDIR); \
		$(RM) -r $(TMPDIR); \
	fi
