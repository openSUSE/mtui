
import requests
from itertools import chain
from  json.decoder import JSONDecodeError

class SMELT(object):
    """ Fetch and parse data from smelt """

    api_url = 'https://maintenance.suse.de/api/incident/'

    def __init__(self, incident_id):
        self.incident = incident_id
        self.job = self._get_incident_data()

    def _get_incident_data(self):
        try:
            smelt = requests.get(self.api_url+str(self.incident))
        except requests.exceptions.ConnectionError:
            smelt = None
        else:
            try:
                smelt = smelt.json()
            except JSONDecodeError:
                smelt = None
        return smelt

    def openqa_links(self):
        comments = self.job['comments']
        links = [z['text'].split('\n') for z in comments if z['who'] == 'sle-qam-openqa']
        if not links:
            self.logger.debug("None known openQA jobs")
            return None
        links = [z.rstrip(')__').split('(')[-1] for z in list(chain.from_iterable(links)) if z.startswith('__Group')]
        self.logger.info("openQA jobs found")
        return links

    def _parse_checkers(self):
        checks = self.job['checkers']['checks']
        # return only checks resuls with data in 'output' key
        valid_checks = {i: j for i, j in {x: [a for a in checks[x] if a['output']] for x in checks}.items() if j}
        return valid_checks

    def pretty_output(self):
        checks = self._parse_checkers()
        if not checks:
            self.logger.debug("No data from SMELT checkers")
            return []
        out = []
        for x, y in checks.items():
            out += ['\n']
            out += ["{} checker:\n".format(x.capitalize())]
            for i in y:
                if 'name' in i:
                    out += ["  product: {}_{} arch: {}\n".format(i['name'], i['version'], i['architecture'])]
                out += ["    " + a + '\n' for a in i['output'].split('\n') if a]
        return out

    def __bool__(self):
        return True if self.job else False
