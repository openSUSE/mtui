""" Module containing SMELT parsing and template fill code """

from datetime import datetime
from itertools import chain
from json.decoder import JSONDecodeError
from logging import getLogger

import requests

from ..messages import RepositoryError
from ..utils import walk

logger = getLogger("mtui.connector.smelt")


class SMELT:
    """
    SMELT Class
    param logger: link to logging object
    param rrid: RequestReviewID instance
    """

    def __init__(self, rrid, apiurl="https://smelt.suse.de/graphql/"):
        self.rrid = rrid
        self.apiurl = apiurl
        self.data = self._get_data()

    def _get_data(self):

        query_incident = f"""{{
  incidents(incidentId: {self.rrid.maintenance_id} ) {{
    edges {{
      node {{
        requestSet(kind: "RR", status_Name_Iexact: "review") {{
          edges {{
            node {{
              comments(who_Username_Iexact: "sle-qam-openqa") {{
                edges {{
                  node {{
                    text
                    when
                  }}
                }}
              }}
              status {{
                name
              }}
            }}
          }}
        }}
        packages {{
          edges {{
            node {{
              name
              }}
            }}
        }}
        repositories {{
          edges {{
            node {{
              name
            }}
          }}
        }}
        comments(who_Username_Iexact: "sle-qam-openqa") {{
          edges {{
            node {{
              text
              when
            }}
          }}
        }}
      }}
    }}
  }}
}}"""

        try:
            inc = requests.get(
                self.apiurl, params={"query": query_incident}, verify=False
            ).json()
        except (requests.exceptions.ConnectionError, JSONDecodeError) as e:
            logger.debug(f"Problem {e} during retrriving incident")
            return None
        if not inc:
            return None
        try:
            inc = walk(inc["data"]["incidents"]["edges"][0]["node"])
        except Exception as e:
            logger.debug(f"Problem {e} during normalize incident")
            return None
        return inc

    def openqa_links(self):
        """" Get openQA links from comments in IBS .. copied to SMELT api:) """
        links = self._comments(self.data)
        if not links:
            logger.debug("None known openQA jobs")
            return None

        links = [
            z.rstrip(")__").split("(")[-1] for z in links if z.startswith("__Group")
        ]
        logger.info("openQA jobs found")
        return links

    def openqa_links_verbose(self):
        links = self._comments(self.data)

        if not links:
            logger.debug("None known openQA jobs")
            return None

        verbose_links = []
        second = False

        for x in links:
            if second:
                second = False
                verbose_links.append("    results: " + x[1:-1])
            if x.startswith("__Group"):
                second = True
                verbose_links.append(x.split("[")[1].split("]")[0] + ":")
                verbose_links.append("  link: " + x.rstrip(")_").split("(")[-1])

        return verbose_links

    @staticmethod
    def _comments(data):

        if not data:
            return None
        if "comments" not in data:
            return None

        comments = [comment for comment in data["comments"] if "when" in comment]

        comments += [
            comment
            for comment in chain.from_iterable(
                c["comments"] for c in (r for r in data["requestSet"])
            )
            if "when" in comment
        ]

        if comments:
            last = sorted(
                comments,
                key=lambda x: datetime.strptime(
                    x["when"].split("+")[0], r"%Y-%m-%dT%H:%M:%S"
                ),
                reverse=True,
            )[0]
        else:
            return None

        return last["text"].split("\n")

    def get_incident_name(self):
        if not self:
            return None
        return sorted([pkg["name"] for pkg in self.data["packages"]], key=len)[0]

    def get_version(self):
        """ Usable only for kernel/live-patching updates, normal updates can have multiple products versions"""

        if not self:
            return None
        # take first repo ..
        base = self.data["repositories"][0]["name"].split(":")[-2].split("-")
        return f"{base[0]}-{base[1]}"

    def __bool__(self):
        if (
            self.data
            == {
                "requestSet": [],
                "packages": [],
                "repositories": [],
                "comments": [],
            }
            or not self.data
        ):
            return False
        return True
