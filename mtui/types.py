import re

class MD5Hash(object):
    def __init__(self, hash_):
        try:
            int(hash_, base = 16)
        except ValueError:
            raise ValueError("{0!r} doesn't look like md5 hexdigest".
                format(hash_))
        else:
            self.hash = hash_

    def __str__(self):
        return str(self.hash)

    def __eq__(self, x):
        if not isinstance(x, MD5Hash):
            raise TypeError("MD5Hash instance expected. Got: {0!r}".
                format(x))

        return self.hash == x.hash
