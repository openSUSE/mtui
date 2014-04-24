.PHONY: help
helps+= help "what you are reading"
help:

	@printf "Targets:\n\n"
	@printf "%-16s - %s\n" $(helps)


.PHONY: check
helps += check "run unit tests"
check:

	nosetests -v tests

.PHONY: checkcover
helps += checkcover "run unit tests with coverage"
checkcover: clean .coverage

.coverage:

	nosetests -v --with-coverage --cover-package=mtui tests

.PHONY: annotate
helps += annotate "annotate source code with execution information"
annotate: .coverage

	find ./mtui -name '*.py' -exec coverage annotate {} \;
# using coverage annotate -d generates shitty filenames

.PHONY: clean
helps += clean ""
clean:

	$(RM) .coverage
	find -name '*.py,cover' -delete
