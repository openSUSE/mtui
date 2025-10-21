import pytest
from mtui import args
from mtui.argparse import ArgsParseFailure
import sys
from io import StringIO
from pathlib import Path

def test_get_parser():
    """
    Test get_parser
    """
    parser = args.get_parser(sys)
    assert parser is not None

    # Check for help argument
    with pytest.raises(ArgsParseFailure):
        parser.parse_args(['-h'])

    with pytest.raises(ArgsParseFailure):
        parser.parse_args(['--help'])

def test_get_parser_args():
    """
    Test get_parser with arguments
    """
    parser = args.get_parser(sys)

    # Test short arguments
    parsed_args = parser.parse_args([
        '-l', 'test_location',
        '-t', '/test/template_dir',
        '-s', 'host1,host2',
        '-w', '600',
        '-n',
        '-d',
        '-c', '/test/config',
        '--smelt_api', 'https://test/smelt_api',
        '-g', 'test_gitea_token',
        '-a', 'SUSE:Maintenance:1:1'
    ])

    assert parsed_args.location == 'test_location'
    assert parsed_args.template_dir == Path('/test/template_dir')
    assert parsed_args.sut[0].print_args() == '-t host1 -t host2'
    assert parsed_args.connection_timeout == 600
    assert parsed_args.noninteractive is True
    assert parsed_args.debug is True
    assert parsed_args.config == Path('/test/config')
    assert parsed_args.smelt_api == 'https://test/smelt_api'
    assert parsed_args.gitea_token == 'test_gitea_token'
    assert str(parsed_args.update.id) == 'SUSE:Maintenance:1:1'

    # Test long arguments
    parsed_args = parser.parse_args([
        '--location', 'test_location_long',
        '--template_dir', '/test/template_dir_long',
        '--sut', 'host3,host4',
        '--connection_timeout', '1200',
        '--noninteractive',
        '--debug',
        '--config', '/test/config_long',
        '--smelt_api', 'https://test/smelt_api_long',
        '--gitea_token', 'test_gitea_token_long',
        '--kernel-review-id', 'SUSE:Maintenance:2:2'
    ])

    assert parsed_args.location == 'test_location_long'
    assert parsed_args.template_dir == Path('/test/template_dir_long')
    assert parsed_args.sut[0].print_args() == '-t host3 -t host4'
    assert parsed_args.connection_timeout == 1200
    assert parsed_args.noninteractive is True
    assert parsed_args.debug is True
    assert parsed_args.config == Path('/test/config_long')
    assert parsed_args.smelt_api == 'https://test/smelt_api_long'
    assert parsed_args.gitea_token == 'test_gitea_token_long'
    assert str(parsed_args.update.id) == 'SUSE:Maintenance:2:2'

    # Test mutually exclusive group
    with pytest.raises(ArgsParseFailure):
        parser.parse_args(['-a', 'SUSE:Maintenance:1:1', '-k', 'SUSE:Maintenance:2:2'])
