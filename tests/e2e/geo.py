"""Country databases for the stats suite: a MaxMind DB file that maps chosen
addresses to chosen countries, laid out as DB-IP's IP to Country Lite, which the pool reads through `[geo]`."""

from mmdb_writer import MMDBWriter
from netaddr import IPSet


def write(path, countries):
    """Writes a database at `path` mapping each IPv4 address of `countries`, {address: (code, name)}, to its
    country. Any other address has no country."""
    writer = MMDBWriter(
        ip_version=4, database_type="DBIP-Country-Lite", languages=["en"], description="PuntoHunter e2e"
    )
    by_country = {}
    for address, country in countries.items():
        by_country.setdefault(country, []).append(f"{address}/32")
    for (code, name), addresses in by_country.items():
        writer.insert_network(IPSet(addresses), {"country": {"iso_code": code, "names": {"en": name}}})
    writer.to_db_file(str(path))
    return path
